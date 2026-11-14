//! Mirrors `packages/ai/src/api/anthropic-messages.ts` — the SSE →
//! `AssistantMessageEvent` mapper (`content_block_*` → text/thinking/toolcall
//! start/delta/end, `message_start`/`message_delta` → usage + stop_reason) and
//! `mapStopReason`.
//!
//! The TS `stream` function keeps a single mutable `output: AssistantMessage`
//! plus a `blocks[]` array carrying per-block scratch state (the streaming
//! `partialJson` buffer for tool calls). The Rust port models that as
//! [`MapperState`]: an `AssistantMessage` grown per event, plus a parallel
//! `Vec<BlockScratch>` tracking each content block's Anthropic `index` and, for
//! tool calls, the `partial_json` accumulation.
//!
//! Tool-call arg parsing invariant (plan §5.15): batch `input_json_delta`
//! snapshots on the same byte/interval cadence as text (the snapshot carries a
//! full message clone and re-parses the accumulated JSON), and do the final
//! authoritative parse on `content_block_stop`. The streaming arm uses
//! [`parse_streaming_json`] so a truncated delta never produces unparseable args
//! — `ToolCallEnd` always carries a real object.

use crate::error::AiError;
use crate::event_stream::AssistantMessageEventStreamProducer;
use crate::providers::anthropic::json_parse::parse_streaming_json;
use crate::providers::anthropic::sse::{AnthropicEvent, SseEventStream};
use crate::providers::{DELTA_FLUSH_BYTES, DELTA_FLUSH_INTERVAL};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, Content, DoneReason, ErrorReason, StopReason,
    TextContent, TextContentType, ThinkingContent, ThinkingContentType, ToolCall, ToolCallType,
    Usage, UsageCost,
};
use std::sync::Arc;
use std::time::Instant;

/// Per-content-block scratch state, mirroring the TS `Block` shape with its
/// `index` + `partialJson` fields.
#[derive(Debug, Clone)]
struct BlockScratch {
    /// The Anthropic `content_block.index` this scratch tracks. The mapper
    /// grows `output.content` in arrival order but blocks may arrive with
    /// non-sequential indices (e.g. interleaved thinking+text on Opus 4.7); we
    /// look up scratch by `index`, not by `content` position.
    anthropic_index: i64,
    /// The position in `output.content` where this block lives. Set on
    /// `content_block_start`, read on every delta/stop.
    content_index: usize,
    /// The block's kind, so deltas can match without re-inspecting the content.
    kind: BlockKind,
    /// For tool-call blocks: the running `partial_json` buffer, appended on
    /// each `input_json_delta`. Empty for text/thinking.
    partial_json: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking { redacted: bool },
    ToolCall,
}

/// The mutable state the mapper advances per SSE event. Mirrors the TS `output`
/// + `blocks` pair.
pub struct MapperState {
    pub output: AssistantMessage,
    blocks: Vec<BlockScratch>,
    /// Marker that the mapper has emitted `start` for the stream. The TS
    /// `stream` pushes `start` right before the event loop; the Rust port
    /// defers it to the first event so a pre-stream failure (auth/SSE) doesn't
    /// deliver a partial `start` with no terminal event.
    started: bool,
    /// Whether the protocol's terminal `message_stop` event arrived. Some
    /// proxy gateways end the stream at `message_stop` WITHOUT a preceding
    /// `message_delta` (which carries `stop_reason`); [`finalize_mapper`] uses
    /// this to default a still-`Pending` stop reason to a normal stop instead
    /// of erroring (documented divergence — see `finalize_mapper`).
    saw_message_end: bool,
    /// Text/thinking deltas batched but not yet emitted, with the content index
    /// they belong to. Every event carries a full `partial: Arc<AssistantMessage>`
    /// snapshot, so one event per wire delta made a long stream quadratic in the
    /// message length — see [`DELTA_FLUSH_BYTES`] for the measured numbers.
    pending_text: String,
    pending_text_index: Option<usize>,
    pending_thinking: String,
    pending_thinking_index: Option<usize>,
    /// Tool-call argument deltas batched but not yet emitted. A snapshot
    /// re-parses the accumulated partial JSON and clones the whole message, so
    /// emitting one per wire delta made large `write`/`edit` arguments
    /// quadratic. The authoritative parse still happens at block stop.
    pending_tool: String,
    pending_tool_index: Option<usize>,
    /// When a batched delta was last emitted, for [`DELTA_FLUSH_INTERVAL`].
    last_flush: Option<tokio::time::Instant>,
}

impl MapperState {
    /// Build a fresh state with an empty assistant message ready to grow.
    /// `timestamp` is ms-since-epoch; callers pass it from the provider.
    pub fn new(
        api: crate::types::Api,
        provider: impl Into<String>,
        model: impl Into<String>,
        timestamp: i64,
    ) -> Self {
        Self {
            output: AssistantMessage::empty(api, provider, model, timestamp),
            blocks: Vec::new(),
            started: false,
            saw_message_end: false,
            pending_text: String::new(),
            pending_text_index: None,
            pending_thinking: String::new(),
            pending_thinking_index: None,
            pending_tool: String::new(),
            pending_tool_index: None,
            last_flush: None,
        }
    }

    /// Whether a pending batch should be emitted now: past the byte threshold,
    /// or older than the flush interval.
    fn should_flush(&self, pending: &str) -> bool {
        pending.len() >= DELTA_FLUSH_BYTES
            || self
                .last_flush
                .is_none_or(|last| last.elapsed() >= DELTA_FLUSH_INTERVAL)
    }

    fn flush_deadline(&self) -> Option<tokio::time::Instant> {
        if !self.pending_text.is_empty()
            || !self.pending_thinking.is_empty()
            || !self.pending_tool.is_empty()
        {
            self.last_flush.map(|last| last + DELTA_FLUSH_INTERVAL)
        } else {
            None
        }
    }

    /// Emit whatever text/thinking is batched. Called at every block boundary
    /// (`content_block_stop`, `message_stop`, the terminal event) and whenever
    /// the stream switches channel, so no delta is dropped or reordered.
    async fn flush_pending(&mut self, prod: &mut AssistantMessageEventStreamProducer) {
        self.flush_text(prod).await;
        self.flush_thinking(prod).await;
        self.flush_tool(prod).await;
    }

    async fn flush_text(&mut self, prod: &mut AssistantMessageEventStreamProducer) {
        if self.pending_text.is_empty() {
            return;
        }
        let Some(content_index) = self.pending_text_index else {
            self.pending_text.clear();
            return;
        };
        let delta = std::mem::take(&mut self.pending_text);
        self.last_flush = Some(tokio::time::Instant::now());
        prod.push(AssistantMessageEvent::TextDelta {
            content_index,
            delta,
            partial: Arc::new(self.output.clone()),
        })
        .await;
    }

    async fn flush_thinking(&mut self, prod: &mut AssistantMessageEventStreamProducer) {
        if self.pending_thinking.is_empty() {
            return;
        }
        let Some(content_index) = self.pending_thinking_index else {
            self.pending_thinking.clear();
            return;
        };
        let delta = std::mem::take(&mut self.pending_thinking);
        self.last_flush = Some(tokio::time::Instant::now());
        prod.push(AssistantMessageEvent::ThinkingDelta {
            content_index,
            delta,
            partial: Arc::new(self.output.clone()),
        })
        .await;
    }

    async fn flush_tool(&mut self, prod: &mut AssistantMessageEventStreamProducer) {
        if self.pending_tool.is_empty() {
            return;
        }
        let Some(content_index) = self.pending_tool_index else {
            self.pending_tool.clear();
            return;
        };
        let delta = std::mem::take(&mut self.pending_tool);
        // Re-parse the accumulated partial JSON once per emitted checkpoint, not
        // once per wire delta. The final parse at block stop stays authoritative.
        let arguments = match self
            .blocks
            .iter()
            .find(|block| block.content_index == content_index)
        {
            Some(scratch) => parse_streaming_json(Some(&scratch.partial_json)),
            None => serde_json::Value::Object(serde_json::Map::new()),
        };
        if let Some(Content::ToolCall(slot)) = self.output.content.get_mut(content_index) {
            slot.arguments = arguments;
        }
        self.last_flush = Some(tokio::time::Instant::now());
        prod.push(AssistantMessageEvent::ToolCallDelta {
            content_index,
            delta,
            partial: Arc::new(self.output.clone()),
        })
        .await;
    }

    async fn ensure_started(&mut self, prod: &mut AssistantMessageEventStreamProducer) {
        if !self.started {
            self.started = true;
            prod.push(AssistantMessageEvent::Start {
                partial: Arc::new(self.output.clone()),
            })
            .await;
        }
    }

    fn find_block_by_anthropic_index(&self, anthropic_index: i64) -> Option<usize> {
        self.blocks
            .iter()
            .position(|b| b.anthropic_index == anthropic_index)
    }

    /// Apply one decoded Anthropic event. Mirrors the per-`event.type` dispatch
    /// in the TS `stream` async IIFE. Returns `Err(AiError)` only on
    /// unrecoverable mapper state (an unexpected block kind for a delta); SSE
    /// parse errors surface earlier, in the decoder. Never panics: a missing
    /// block for an index is a no-op (the TS `findIndex` returns -1 and the
    /// delta is skipped).
    pub async fn apply(
        &mut self,
        event: &AnthropicEvent,
        prod: &mut AssistantMessageEventStreamProducer,
    ) -> Result<(), AiError> {
        let AnthropicEvent::Message {
            event_type,
            payload,
        } = event
        else {
            return Ok(()); // Skipped events are a mapper no-op.
        };

        // The TS loop only enters the match arms for event names it recognizes;
        // `iterate_anthropic_events` already filtered to ANTHROPIC_MESSAGE_EVENTS,
        // so we dispatch on the payload `type` (which equals the SSE event name).
        let checkpoint_metadata = matches!(event_type.as_str(), "message_start" | "message_delta");
        match event_type.as_str() {
            "message_start" => self.apply_message_start(payload, prod).await,
            "content_block_start" => self.apply_content_block_start(payload, prod).await,
            "content_block_delta" => self.apply_content_block_delta(payload, prod).await?,
            "content_block_stop" => self.apply_content_block_stop(payload, prod).await,
            "message_delta" => self.apply_message_delta(payload, prod).await,
            "message_stop" => self.saw_message_end = true,
            _ => {}
        }
        if checkpoint_metadata {
            prod.commit_metadata(&self.output);
        }
        Ok(())
    }

    async fn apply_message_start(
        &mut self,
        payload: &serde_json::Value,
        prod: &mut AssistantMessageEventStreamProducer,
    ) {
        self.ensure_started(prod).await;
        // `event.message.id` — response id.
        if let Some(id) = payload
            .pointer("/message/id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
        {
            self.output.response_id = Some(id);
        }

        // Initial usage (input/cacheRead/cacheWrite; output may be 0 here and
        // updated on the final message_delta). Preserves input_tokens from
        // message_start when a proxy omits it in message_delta.
        if let Some(usage) = payload.pointer("/message/usage") {
            let input = usage
                .get("input_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let output = usage
                .get("output_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let cache_read = usage
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let cache_write = usage
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            // `cache_creation.ephemeral_1h_input_tokens` — the subset of
            // cache_creation written with 1h retention (cost calculation
            // charges these at 2× input).
            let cache_write_1h = usage
                .pointer("/cache_creation/ephemeral_1h_input_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            self.output.usage.input = input;
            self.output.usage.output = output;
            self.output.usage.cache_read = cache_read;
            self.output.usage.cache_write = cache_write;
            self.output.usage.cache_write_1h = if cache_write_1h > 0 {
                Some(cache_write_1h)
            } else {
                None
            };
            // Anthropic doesn't report total_tokens; compute from components
            // (mirrors the TS `input + output + cacheRead + cacheWrite` line).
            self.output.usage.total_tokens = input + output + cache_read + cache_write;
            // Cost is recomputed in the provider after the model is known; the
            // mapper zeroes cost here so the provider's final pass owns it.
            self.output.usage.cost = UsageCost::default();
        }
    }

    async fn apply_content_block_start(
        &mut self,
        payload: &serde_json::Value,
        prod: &mut AssistantMessageEventStreamProducer,
    ) {
        // A new block implies the previous one is done. `content_block_stop`
        // normally flushed already; this covers a stream that opens a block
        // without closing the last one.
        self.flush_pending(prod).await;
        self.ensure_started(prod).await;
        let anthropic_index = payload.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
        let Some(block) = payload.get("content_block") else {
            return;
        };
        let kind = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "text" => {
                let text = block
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let content = Content::Text(TextContent {
                    kind: TextContentType,
                    text,
                    text_signature: None,
                });
                self.output.content.push(content);
                let content_index = self.output.content.len() - 1;
                self.blocks.push(BlockScratch {
                    anthropic_index,
                    content_index,
                    kind: BlockKind::Text,
                    partial_json: String::new(),
                });
                prod.push(AssistantMessageEvent::TextStart {
                    content_index,
                    partial: Arc::new(self.output.clone()),
                })
                .await;
            }
            "thinking" => {
                let thinking = block
                    .get("thinking")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let signature = block
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let content = Content::Thinking(ThinkingContent {
                    kind: ThinkingContentType,
                    thinking,
                    thinking_signature: if signature.is_empty() {
                        None
                    } else {
                        Some(signature)
                    },
                    redacted: false,
                });
                self.output.content.push(content);
                let content_index = self.output.content.len() - 1;
                self.blocks.push(BlockScratch {
                    anthropic_index,
                    content_index,
                    kind: BlockKind::Thinking { redacted: false },
                    partial_json: String::new(),
                });
                prod.push(AssistantMessageEvent::ThinkingStart {
                    content_index,
                    partial: Arc::new(self.output.clone()),
                })
                .await;
            }
            "redacted_thinking" => {
                let data = block
                    .get("data")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let content = Content::Thinking(ThinkingContent {
                    kind: ThinkingContentType,
                    // Mirrors the TS: a fixed marker, the opaque payload lives
                    // in `thinking_signature` so it round-trips back to
                    // `redacted_thinking` on the next turn.
                    thinking: "[Reasoning redacted]".to_string(),
                    thinking_signature: Some(data),
                    redacted: true,
                });
                self.output.content.push(content);
                let content_index = self.output.content.len() - 1;
                self.blocks.push(BlockScratch {
                    anthropic_index,
                    content_index,
                    kind: BlockKind::Thinking { redacted: true },
                    partial_json: String::new(),
                });
                prod.push(AssistantMessageEvent::ThinkingStart {
                    content_index,
                    partial: Arc::new(self.output.clone()),
                })
                .await;
            }
            "tool_use" => {
                let id = block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                // `input` is usually `{}` on block_start (streaming fills it
                // via deltas); keep whatever's present as the initial args.
                let args = block
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                let content = Content::tool_call(id, name, args);
                self.output.content.push(content);
                let content_index = self.output.content.len() - 1;
                self.blocks.push(BlockScratch {
                    anthropic_index,
                    content_index,
                    kind: BlockKind::ToolCall,
                    partial_json: String::new(),
                });
                prod.push(AssistantMessageEvent::ToolCallStart {
                    content_index,
                    partial: Arc::new(self.output.clone()),
                })
                .await;
            }
            // image / unknown content blocks are not emitted by the Anthropic
            // Messages streaming API on the assistant side — skip (mirrors TS).
            _ => {}
        }
    }

    async fn apply_content_block_delta(
        &mut self,
        payload: &serde_json::Value,
        prod: &mut AssistantMessageEventStreamProducer,
    ) -> Result<(), AiError> {
        let anthropic_index = payload.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
        let Some(delta) = payload.get("delta") else {
            return Ok(());
        };
        let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");

        let Some(scratch_index) = self.find_block_by_anthropic_index(anthropic_index) else {
            // Delta for an unknown index (block_start missed/truncated) — skip
            // rather than panic, matching the TS `if (block && ...)` guards.
            return Ok(());
        };
        // Only the tool-call arm needs the scratch buffer, and `flush_pending`
        // needs `&mut self` — so read the two scalars here and take the buffer
        // borrow inside that arm instead of holding one across the whole match.
        let content_index = self.blocks[scratch_index].content_index;
        let block_kind = self.blocks[scratch_index].kind;

        match (block_kind, delta_type) {
            (BlockKind::Text, "text_delta") => {
                let text = delta
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(Content::Text(slot)) = self.output.content.get_mut(content_index) {
                    slot.text.push_str(&text);
                }
                // Channel switch: a batched tool-call delta must reach the
                // consumer before this text delta.
                self.flush_tool(prod).await;
                // Batch: `partial` is a full message clone per event.
                self.pending_text_index = Some(content_index);
                self.pending_text.push_str(&text);
                if self.should_flush(&self.pending_text) {
                    self.flush_text(prod).await;
                }
            }
            (BlockKind::Thinking { redacted: false }, "thinking_delta") => {
                let thinking = delta
                    .get("thinking")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(Content::Thinking(slot)) = self.output.content.get_mut(content_index) {
                    slot.thinking.push_str(&thinking);
                }
                // Keep channel order strict: text and tool-call deltas already
                // streamed must reach the consumer before the first thinking
                // delta of a new block.
                self.flush_text(prod).await;
                self.flush_tool(prod).await;
                self.pending_thinking_index = Some(content_index);
                self.pending_thinking.push_str(&thinking);
                if self.should_flush(&self.pending_thinking) {
                    self.flush_thinking(prod).await;
                }
            }
            (BlockKind::Thinking { redacted: false }, "signature_delta") => {
                // Append to the existing signature (initializing empty to "").
                let sig = delta
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if let Some(Content::Thinking(slot)) = self.output.content.get_mut(content_index) {
                    let current = slot.thinking_signature.take().unwrap_or_default();
                    slot.thinking_signature = Some(format!("{current}{sig}"));
                }
            }
            (BlockKind::ToolCall, "input_json_delta") => {
                let partial = delta
                    .get("partial_json")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                // A tool-call block is a new channel; flush what is batched so
                // the consumer never sees tool events before the text that
                // preceded them.
                self.flush_text(prod).await;
                self.flush_thinking(prod).await;
                if self.pending_tool_index != Some(content_index) {
                    self.flush_tool(prod).await;
                }
                self.blocks[scratch_index].partial_json.push_str(&partial);
                self.pending_tool_index = Some(content_index);
                self.pending_tool.push_str(&partial);
                // Batch the live-args snapshot: re-parsing the accumulated JSON
                // and cloning the message once per wire delta is quadratic for
                // large `write`/`edit` arguments. The final parse at block stop
                // remains authoritative.
                if self.should_flush(&self.pending_tool) {
                    self.flush_tool(prod).await;
                }
            }
            // Redacted-thinking blocks receive no deltas; unknown delta types
            // are ignored (forward-compat for future delta variants).
            _ => {}
        }
        Ok(())
    }

    async fn apply_content_block_stop(
        &mut self,
        payload: &serde_json::Value,
        prod: &mut AssistantMessageEventStreamProducer,
    ) {
        let anthropic_index = payload.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
        let Some(scratch_index) = self.find_block_by_anthropic_index(anthropic_index) else {
            return;
        };
        // Block boundary: emit the batched tail before the `*End` event that
        // closes the block, otherwise the tail would be visible in the final
        // message but never in the delta stream. Must happen before `scratch`
        // takes its mutable borrow of `self.blocks`.
        self.flush_pending(prod).await;
        let scratch = &mut self.blocks[scratch_index];
        let content_index = scratch.content_index;
        match scratch.kind {
            BlockKind::Text => {
                let content = match &self.output.content[content_index] {
                    Content::Text(t) => t.text.clone(),
                    _ => String::new(),
                };
                prod.push(AssistantMessageEvent::TextEnd {
                    content_index,
                    content,
                    partial: Arc::new(self.output.clone()),
                })
                .await;
            }
            BlockKind::Thinking { redacted } => {
                let thinking_text = match &self.output.content[content_index] {
                    Content::Thinking(t) => t.thinking.clone(),
                    _ => String::new(),
                };
                // The final `partialJson` parse was already done on each delta;
                // for tool calls the stop event does the authoritative parse.
                // For thinking there's no JSON; just emit the accumulated text.
                let _ = redacted;
                prod.push(AssistantMessageEvent::ThinkingEnd {
                    content_index,
                    content: thinking_text,
                    partial: Arc::new(self.output.clone()),
                })
                .await;
            }
            BlockKind::ToolCall => {
                // Final authoritative parse. Mirrors the TS
                // `block.arguments = parseStreamingJson(block.partialJson)`.
                let final_args = parse_streaming_json(Some(&scratch.partial_json));
                let tool_call = match &self.output.content[content_index] {
                    Content::ToolCall(tc) => ToolCall {
                        kind: ToolCallType,
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        arguments: final_args,
                        thought_signature: tc.thought_signature.clone(),
                        namespace: tc.namespace.clone(),
                        recorded: tc.recorded.clone(),
                    },
                    _ => return,
                };
                if let Some(Content::ToolCall(slot)) = self.output.content.get_mut(content_index) {
                    slot.arguments = tool_call.arguments.clone();
                }
                prod.push(AssistantMessageEvent::ToolCallEnd {
                    content_index,
                    tool_call,
                    partial: Arc::new(self.output.clone()),
                })
                .await;
            }
        }
    }

    async fn apply_message_delta(
        &mut self,
        payload: &serde_json::Value,
        prod: &mut AssistantMessageEventStreamProducer,
    ) {
        let _ = prod; // message_delta never pushes events; it mutates output only.
                      // `delta.stop_reason` → mapStopReason + rawStopReason.
        if let Some(stop_reason) = payload
            .pointer("/delta/stop_reason")
            .and_then(|v| v.as_str())
        {
            self.output.raw_stop_reason = Some(stop_reason.to_string());
            let stop_details = payload.pointer("/delta/stop_details");
            match map_stop_reason(stop_reason, stop_details) {
                Ok(MappedStop {
                    stop_reason,
                    error_message,
                }) => {
                    self.output.stop_reason = stop_reason;
                    if let Some(msg) = error_message {
                        self.output.error_message = Some(msg);
                    }
                }
                Err(e) => {
                    // TS throws on unhandled stop reasons; the Rust port records
                    // it as an error message + Error stop_reason so the stream
                    // still terminates (the provider loop converts the
                    // post-stream Error check).
                    self.output.stop_reason = StopReason::Error;
                    self.output.error_message = Some(e.to_string());
                }
            }
        }

        // Usage update — only fields that are present (not null), mirroring the
        // TS field-by-field null checks. Preserves message_start input_tokens
        // when the proxy omits them here.
        if let Some(usage) = payload.get("usage") {
            if let Some(v) = usage.get("input_tokens").and_then(|v| v.as_i64()) {
                self.output.usage.input = v;
            }
            if let Some(v) = usage.get("output_tokens").and_then(|v| v.as_i64()) {
                self.output.usage.output = v;
            }
            if let Some(v) = usage
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_i64())
            {
                self.output.usage.cache_read = v;
            }
            if let Some(v) = usage
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_i64())
            {
                self.output.usage.cache_write = v;
            }
            // Reasoning tokens — a subset of output_tokens, reported via
            // `output_tokens_details.thinking_tokens` (the SDK type omits the
            // field; the TS reads it through a narrow cast). Mirrors that.
            if let Some(thinking_tokens) = usage
                .pointer("/output_tokens_details/thinking_tokens")
                .and_then(|v| v.as_i64())
            {
                self.output.usage.reasoning = Some(thinking_tokens);
            }
        }
        // Anthropic doesn't provide total_tokens; recompute from components
        // unconditionally (mirrors TS line 741, OUTSIDE the `if (event.usage)`
        // block). When usage is absent the components are unchanged, so this
        // is a no-op — but it stays faithful to the source's control flow.
        self.output.usage.total_tokens = self.output.usage.input
            + self.output.usage.output
            + self.output.usage.cache_read
            + self.output.usage.cache_write;
    }
}

/// The result of mapping an Anthropic stop reason. Mirrors the TS return shape
/// `{ stopReason, errorMessage? }`.
pub struct MappedStop {
    pub stop_reason: StopReason,
    pub error_message: Option<String>,
}

/// Map an Anthropic `stop_reason` to pi's `StopReason`, optionally carrying an
/// error message (`refusal`/`sensitive`). Mirrors TS `mapStopReason`.
///
/// Returns `Err(AiError::Provider)` for an unhandled stop reason so the caller
/// can surface it; the TS source `throw`s, which the Rust port translates to an
/// `AiError` rather than a panic.
pub fn map_stop_reason(
    reason: &str,
    stop_details: Option<&serde_json::Value>,
) -> Result<MappedStop, AiError> {
    match reason {
        "end_turn" => Ok(MappedStop {
            stop_reason: StopReason::Stop,
            error_message: None,
        }),
        "max_tokens" => Ok(MappedStop {
            stop_reason: StopReason::Length,
            error_message: None,
        }),
        "tool_use" => Ok(MappedStop {
            stop_reason: StopReason::ToolUse,
            error_message: None,
        }),
        "refusal" => {
            let explanation = stop_details
                .and_then(|d| d.get("explanation"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "The model refused to complete the request".to_string());
            Ok(MappedStop {
                stop_reason: StopReason::Error,
                error_message: Some(explanation),
            })
        }
        // pause_turn / stop_sequence both collapse to Stop (mirrors TS).
        "pause_turn" | "stop_sequence" => Ok(MappedStop {
            stop_reason: StopReason::Stop,
            error_message: None,
        }),
        "sensitive" => Ok(MappedStop {
            stop_reason: StopReason::Error,
            error_message: Some("Provider stopped with: sensitive".to_string()),
        }),
        other => Err(AiError::Provider {
            code: "unhandled_stop_reason".to_string(),
            message: format!("Unhandled stop reason: {other}"),
        }),
    }
}

/// Run the mapper over a live SSE event stream, pushing
/// `AssistantMessageEvent`s to `prod` as each Anthropic event arrives, and
/// emit the terminal `Done`/`Error` event when the stream ends.
///
/// Mirrors the TS `stream` async IIFE's event loop + terminal handling. A
/// missing stop reason (stream ended with `Pending`) is converted to an
/// `Error` event; an aborted signal surfaces as `Error { reason: Aborted }`.
/// `cost_fn` is called on the final `output.usage` before the terminal event
/// so the message carries the model-aware cost (the TS calls `calculateCost`
/// inline; the Rust port defers to the provider, which knows the `Model`).
#[derive(Debug, Default, Clone, Copy)]
pub struct MapperDiagnostics {
    pub sse_events: u64,
    pub pull_ns: u64,
    pub parse_ns: u64,
    pub apply_ns: u64,
}

/// Run the mapper without diagnostic timing overhead.
pub async fn run_mapper<F>(
    stream: &mut SseEventStream,
    prod: &mut AssistantMessageEventStreamProducer,
    state: &mut MapperState,
    cost_fn: F,
) where
    F: Fn(&Usage) -> UsageCost,
{
    run_mapper_impl(stream, prod, state, cost_fn, None).await;
}

/// Run the mapper and account for raw SSE pulls, JSON decoding, and state
/// application separately. This is diagnostics-only; the normal path uses
/// `run_mapper` and does not take these timestamps.
pub async fn run_mapper_with_diagnostics<F>(
    stream: &mut SseEventStream,
    prod: &mut AssistantMessageEventStreamProducer,
    state: &mut MapperState,
    cost_fn: F,
    diagnostics: &mut MapperDiagnostics,
) where
    F: Fn(&Usage) -> UsageCost,
{
    run_mapper_impl(stream, prod, state, cost_fn, Some(diagnostics)).await;
}

async fn run_mapper_impl<F>(
    stream: &mut SseEventStream,
    prod: &mut AssistantMessageEventStreamProducer,
    state: &mut MapperState,
    cost_fn: F,
    mut diagnostics: Option<&mut MapperDiagnostics>,
) where
    F: Fn(&Usage) -> UsageCost,
{
    loop {
        let pull_started = diagnostics.is_some().then(Instant::now);
        let next = stream.next_event_before(state.flush_deadline()).await;
        if let Some(diagnostics) = diagnostics.as_deref_mut() {
            diagnostics.pull_ns += elapsed_ns(pull_started);
        }
        let Some(next) = next else {
            state.flush_pending(prod).await;
            continue;
        };
        match next {
            Ok(None) => break,
            Ok(Some(sse_frame)) => {
                if let Some(diagnostics) = diagnostics.as_deref_mut() {
                    diagnostics.sse_events += 1;
                }
                let parse_started = diagnostics.is_some().then(Instant::now);
                let event =
                    match crate::providers::anthropic::sse::parse_anthropic_event(&sse_frame) {
                        Ok(e) => e,
                        Err(err) => {
                            if let Some(diagnostics) = diagnostics.as_deref_mut() {
                                diagnostics.parse_ns += elapsed_ns(parse_started);
                            }
                            emit_terminal_error(prod, state, err.to_string(), false).await;
                            return;
                        }
                    };
                if let Some(diagnostics) = diagnostics.as_deref_mut() {
                    diagnostics.parse_ns += elapsed_ns(parse_started);
                }
                let apply_started = diagnostics.is_some().then(Instant::now);
                if let Err(err) = state.apply(&event, prod).await {
                    if let Some(diagnostics) = diagnostics.as_deref_mut() {
                        diagnostics.apply_ns += elapsed_ns(apply_started);
                    }
                    emit_terminal_error(prod, state, err.to_string(), false).await;
                    return;
                }
                if let Some(diagnostics) = diagnostics.as_deref_mut() {
                    diagnostics.apply_ns += elapsed_ns(apply_started);
                }
                // `message_stop` is Anthropic's protocol-level terminal
                // marker. A few proxies leave the HTTP body open or emit an
                // invalid trailing chunk after this frame; finalize immediately.
                if event.event_type() == Some("message_stop") {
                    finalize_mapper(prod, state, cost_fn).await;
                    return;
                }
            }
            Err(err) => {
                let aborted = matches!(err, AiError::Abort { .. });
                emit_terminal_error(prod, state, err.to_string(), aborted).await;
                return;
            }
        }
    }

    finalize_mapper(prod, state, cost_fn).await;
}

fn elapsed_ns(started: Option<Instant>) -> u64 {
    started
        .map(|started| started.elapsed().as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or_default()
}

/// Emit the terminal `Done`/`Error` event for a normally-ended stream. Mirrors
/// the TS post-loop checks (lines 747-759): `pending` stop reason → error;
/// `aborted`/`error` → error with the carried message; otherwise `done` with
/// the model-aware cost applied.
///
/// Extracted from [`run_mapper`] so tests can drive the mapper over a
/// pre-decoded `Vec<AnthropicEvent>` (no `reqwest::Response` needed) and still
/// exercise the terminal logic.
pub async fn finalize_mapper<F>(
    prod: &mut AssistantMessageEventStreamProducer,
    state: &mut MapperState,
    cost_fn: F,
) where
    F: Fn(&Usage) -> UsageCost,
{
    // A stream that reached the protocol's terminal `message_stop` without a
    // `message_delta` is a normal stop — some proxy gateways (e.g. the
    // anthropic-messages endpoints of DeepSeek/GLM relays) omit `message_delta`
    // entirely. The TS check throws "Anthropic stream ended without a stop
    // reason" here, which would reject those gateways; v1 defaults the still-
    // `Pending` stop reason to a normal `Stop` instead (documented divergence —
    // a completed `message_stop` is authoritative, and `message_delta` only
    // carries usage + stop_reason). The Pending-error is preserved for streams
    // that end WITHOUT `message_stop` (the SSE closed early).
    if state.output.stop_reason == StopReason::Pending && state.saw_message_end {
        state.output.stop_reason = StopReason::Stop;
    }
    if state.output.stop_reason == StopReason::Pending {
        emit_terminal_error(
            prod,
            state,
            "Anthropic stream ended without a stop reason".to_string(),
            false,
        )
        .await;
        return;
    }
    if matches!(
        state.output.stop_reason,
        StopReason::Aborted | StopReason::Error
    ) {
        let aborted = matches!(state.output.stop_reason, StopReason::Aborted);
        let msg = state
            .output
            .error_message
            .clone()
            .unwrap_or_else(|| "An unknown error occurred".to_string());
        emit_terminal_error(prod, state, msg, aborted).await;
        return;
    }

    // Success terminal. Apply the model-aware cost last.
    state.output.usage.cost = cost_fn(&state.output.usage);
    let reason = match state.output.stop_reason {
        StopReason::Stop => DoneReason::Stop,
        StopReason::Length => DoneReason::Length,
        StopReason::ToolUse => DoneReason::ToolUse,
        StopReason::Deferred => DoneReason::Deferred,
        _ => DoneReason::Stop, // unreachable given the checks above.
    };
    prod.push(AssistantMessageEvent::Done {
        reason,
        message: state.output.clone(),
    })
    .await;
}

/// Emit the terminal `Error` event for the stream, stamping `output.stop_reason`
/// + `error_message` and pushing an `AssistantMessageEvent::Error`. Mirrors the
/// TS `catch` block's `output.stopReason = ...; output.errorMessage = ...;
/// stream.push({ type: "error", ... })`.
///
/// Extracted as a `pub` helper so the provider's pre-stream catch (auth
/// failure / HTTP error / abort) reuses the same terminal shape the mapper's
/// in-stream catch uses, keeping the consumer's `result()` contract uniform.
pub async fn emit_terminal_error(
    prod: &mut AssistantMessageEventStreamProducer,
    state: &mut MapperState,
    message: String,
    aborted: bool,
) {
    state.output.stop_reason = if aborted {
        StopReason::Aborted
    } else {
        StopReason::Error
    };
    state.output.error_message = Some(message);
    // Surface whatever streamed before the failure; the batched tail would
    // otherwise be lost from the delta stream.
    state.flush_pending(prod).await;
    let reason = if aborted {
        ErrorReason::Aborted
    } else {
        ErrorReason::Error
    };
    prod.push(AssistantMessageEvent::Error {
        reason,
        error: state.output.clone(),
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_stream::create_assistant_message_event_stream;
    use crate::providers::anthropic::sse::{parse_anthropic_event, ServerSentEvent};
    use crate::types::{Api, Content, StopReason};
    use serde_json::json;

    /// Build a `ServerSentEvent` frame from an event name + raw data string.
    fn frame(event: &str, data: &str) -> ServerSentEvent {
        ServerSentEvent {
            event: Some(event.to_string()),
            data: data.to_string(),
            raw: vec![format!("event: {event}\ndata: {data}")],
        }
    }

    /// A JSON object literal as a `&str`, for inline event data.
    fn j(value: &serde_json::Value) -> String {
        serde_json::to_string(value).unwrap()
    }

    struct Run {
        tags: Vec<&'static str>,
        result: AssistantMessage,
    }

    /// Drive a fixture of (event, data) frames through the mapper + finalizer,
    /// returning the emitted event tags and the terminal `AssistantMessage`.
    async fn run_fixture(frames: Vec<ServerSentEvent>) -> Run {
        let (mut prod, stream) = create_assistant_message_event_stream();
        let mut state =
            MapperState::new(Api::AnthropicMessages, "anthropic", "claude-haiku-4-5", 0);
        for f in &frames {
            let event = parse_anthropic_event(f).expect("event parses");
            state.apply(&event, &mut prod).await.expect("apply");
        }
        // finalize_mapper applies the model-aware cost; tests pass a zero cost
        // fn since they assert usage fields, not dollar amounts.
        finalize_mapper(&mut prod, &mut state, |_| UsageCost::default()).await;

        drop(prod);
        let mut stream = stream;
        let mut tags = Vec::new();
        while let Some(ev) = stream.next().await {
            tags.push(ev.type_tag());
        }
        let result = stream.result().await.expect("terminal result");
        Run { tags, result }
    }

    /// Frame list for `count` one-byte text deltas on block 0, closed by a
    /// normal stop. Used by the batching tests below.
    fn text_delta_frames(count: usize) -> Vec<ServerSentEvent> {
        let mut frames = vec![
            frame(
                "message_start",
                &j(&json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_batch",
                        "usage": {
                            "input_tokens": 1,
                            "output_tokens": 0,
                            "cache_read_input_tokens": 0,
                            "cache_creation_input_tokens": 0,
                        },
                    },
                })),
            ),
            frame(
                "content_block_start",
                &j(&json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "text", "text": "" },
                })),
            ),
        ];
        for _ in 0..count {
            frames.push(frame(
                "content_block_delta",
                &j(&json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "text_delta", "text": "x" },
                })),
            ));
        }
        frames.push(frame(
            "content_block_stop",
            &j(&json!({ "type": "content_block_stop", "index": 0 })),
        ));
        frames.push(frame(
            "message_delta",
            &j(&json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn" },
                "usage": { "output_tokens": count },
            })),
        ));
        frames.push(frame(
            "message_stop",
            &j(&json!({ "type": "message_stop" })),
        ));
        frames
    }

    /// The Anthropic mapper carried the same per-delta full-message clone as the
    /// OpenAI one, so it had the same quadratic cost. Batching is shared policy
    /// (`providers::DELTA_FLUSH_BYTES`), and the boundary flush at
    /// `content_block_stop` must still deliver the tail.
    #[tokio::test]
    async fn text_deltas_are_batched_without_losing_the_tail() {
        let count = 500usize;
        let run = run_fixture(text_delta_frames(count)).await;

        let deltas = run.tags.iter().filter(|tag| **tag == "text_delta").count();
        assert!(
            deltas <= count / DELTA_FLUSH_BYTES + 5,
            "{count} one-byte deltas must coalesce into ~{}/{DELTA_FLUSH_BYTES} events, got {deltas}",
            count
        );
        assert!(deltas >= 1, "at least one delta must be emitted");

        // Lossless: the finalized message holds every byte.
        assert!(
            matches!(&run.result.content[0], Content::Text(t) if t.text.len() == count),
            "final message must hold all {count} bytes"
        );
        // The block boundary flush ran before `TextEnd`.
        let end_at = run
            .tags
            .iter()
            .position(|tag| *tag == "text_end")
            .expect("a text_end was emitted");
        let last_delta_at = run
            .tags
            .iter()
            .rposition(|tag| *tag == "text_delta")
            .expect("a text_delta was emitted");
        assert!(
            last_delta_at < end_at,
            "the batched tail must be flushed before text_end: {:?}",
            run.tags
        );
    }

    #[tokio::test]
    async fn a_short_text_block_still_emits_its_delta() {
        // Well under the byte threshold: only the boundary flush can deliver it.
        let run = run_fixture(text_delta_frames(1)).await;
        assert_eq!(
            run.tags.iter().filter(|t| **t == "text_delta").count(),
            1,
            "a one-byte block must still produce one delta: {:?}",
            run.tags
        );
        assert!(matches!(&run.result.content[0], Content::Text(t) if t.text == "x"));
    }

    // Mirrors `anthropic-sse-parsing.test.ts::repairs malformed SSE JSON and
    // malformed streamed tool JSON`. The partial_json delta carries `\H` (an
    // invalid JSON escape) and `\t` (a valid escape). The mapper must repair
    // both layers (outer SSE JSON + inner streaming tool JSON) and end with
    // `stop_reason: ToolUse` + parseable args.
    #[tokio::test]
    async fn repairs_malformed_streamed_tool_json() {
        // TS `String.raw` keeps the backslashes literal: `\H` and `\t` are
        // backslash-H and backslash-t in the raw bytes, NOT an invalid escape
        // pre-decode. The Rust raw string r#"..."# reproduces that exactly.
        let malformed_delta = r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"A\H\",\"text\":\"col1\tcol2\"}"}}"#;

        let frames = vec![
            frame(
                "message_start",
                &j(&json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_test",
                        "usage": {
                            "input_tokens": 12,
                            "output_tokens": 0,
                            "cache_read_input_tokens": 0,
                            "cache_creation_input_tokens": 0,
                        },
                    },
                })),
            ),
            frame(
                "content_block_start",
                &j(&json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "tool_use", "id": "toolu_test", "name": "edit", "input": {} },
                })),
            ),
            frame("content_block_delta", malformed_delta),
            frame(
                "content_block_stop",
                &j(&json!({ "type": "content_block_stop", "index": 0 })),
            ),
            frame(
                "message_delta",
                &j(&json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "tool_use" },
                    "usage": {
                        "input_tokens": 12,
                        "output_tokens": 5,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0,
                    },
                })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];

        let run = run_fixture(frames).await;
        assert_eq!(run.result.stop_reason, StopReason::ToolUse);
        assert!(run.result.error_message.is_none());

        let toolcall = run
            .result
            .content
            .iter()
            .find_map(|c| match c {
                Content::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .expect("a tool call block");
        assert_eq!(
            toolcall.arguments,
            json!({ "path": "A\\H", "text": "col1\tcol2" })
        );
        // Event sequence: start, toolcall_start, toolcall_delta, toolcall_end, done.
        assert_eq!(
            run.tags,
            vec![
                "start",
                "toolcall_start",
                "toolcall_delta",
                "toolcall_end",
                "done"
            ]
        );
    }

    // Mirrors `preserves content from content_block_start events` — text +
    // thinking blocks retain their initial content and accumulate deltas;
    // signature_delta appends to the thinking signature.
    #[tokio::test]
    async fn preserves_content_from_content_block_start() {
        let frames = vec![
            frame(
                "message_start",
                &j(&json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_initial_content",
                        "usage": { "input_tokens": 12, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 },
                    },
                })),
            ),
            frame(
                "content_block_start",
                &j(
                    &json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "Initial text" } }),
                ),
            ),
            frame(
                "content_block_delta",
                &j(
                    &json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": " plus delta" } }),
                ),
            ),
            frame(
                "content_block_stop",
                &j(&json!({ "type": "content_block_stop", "index": 0 })),
            ),
            frame(
                "content_block_start",
                &j(
                    &json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "thinking", "thinking": "Initial thinking", "signature": "initial signature" } }),
                ),
            ),
            frame(
                "content_block_delta",
                &j(
                    &json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "thinking_delta", "thinking": " plus delta" } }),
                ),
            ),
            frame(
                "content_block_delta",
                &j(
                    &json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "signature_delta", "signature": " plus delta" } }),
                ),
            ),
            frame(
                "content_block_stop",
                &j(&json!({ "type": "content_block_stop", "index": 1 })),
            ),
            frame(
                "message_delta",
                &j(&json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn" },
                    "usage": { "input_tokens": 12, "output_tokens": 5, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 },
                })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];

        let run = run_fixture(frames).await;
        assert_eq!(run.result.stop_reason, StopReason::Stop);
        let text = match &run.result.content[0] {
            Content::Text(t) => t,
            _ => panic!("first block is text"),
        };
        assert_eq!(text.text, "Initial text plus delta");
        let thinking = match &run.result.content[1] {
            Content::Thinking(t) => t,
            _ => panic!("second block is thinking"),
        };
        assert_eq!(thinking.thinking, "Initial thinking plus delta");
        assert_eq!(
            thinking.thinking_signature.as_deref(),
            Some("initial signature plus delta")
        );
        assert!(!thinking.redacted);
    }

    // Mirrors `preserves refusal stop details from message_delta` — refusal
    // stop_reason maps to Error with the explanation.
    #[tokio::test]
    async fn preserves_refusal_stop_details() {
        let explanation = "This request triggered restrictions on violative cyber content and was blocked under Anthropic's Usage Policy.";
        let frames = vec![
            frame(
                "message_start",
                &j(&json!({
                    "type": "message_start",
                    "message": { "id": "msg_01XFUDYJgAACzvnptvVoYEL", "usage": { "input_tokens": 412, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } },
                })),
            ),
            frame(
                "message_delta",
                &j(&json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "refusal", "stop_details": { "type": "refusal", "category": "cyber", "explanation": explanation } },
                    "usage": { "input_tokens": 412, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 },
                })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];

        let run = run_fixture(frames).await;
        assert_eq!(run.result.stop_reason, StopReason::Error);
        assert_eq!(run.result.raw_stop_reason.as_deref(), Some("refusal"));
        assert_eq!(run.result.error_message.as_deref(), Some(explanation));
        // Refusal emits an Error terminal, not Done.
        assert_eq!(run.tags.last().copied(), Some("error"));
    }

    // Mirrors `preserves sensitive stop reasons with a descriptive error
    // message`.
    #[tokio::test]
    async fn preserves_sensitive_stop_reasons() {
        let frames = vec![
            frame(
                "message_start",
                &j(
                    &json!({ "type": "message_start", "message": { "id": "msg_sensitive", "usage": { "input_tokens": 12, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } } }),
                ),
            ),
            frame(
                "message_delta",
                &j(
                    &json!({ "type": "message_delta", "delta": { "stop_reason": "sensitive" }, "usage": { "input_tokens": 12, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } }),
                ),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];

        let run = run_fixture(frames).await;
        assert_eq!(run.result.stop_reason, StopReason::Error);
        assert_eq!(run.result.raw_stop_reason.as_deref(), Some("sensitive"));
        assert_eq!(
            run.result.error_message.as_deref(),
            Some("Provider stopped with: sensitive")
        );
    }

    // Mirrors `treats message_delta without usage as a no-op for usage
    // accumulation` — usage retains message_start values; total recomputed
    // from components.
    #[tokio::test]
    async fn message_delta_without_usage_noop() {
        let frames = vec![
            frame(
                "message_start",
                &j(
                    &json!({ "type": "message_start", "message": { "id": "msg_test", "usage": { "input_tokens": 12, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } } }),
                ),
            ),
            frame(
                "content_block_start",
                &j(
                    &json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
                ),
            ),
            frame(
                "content_block_delta",
                &j(
                    &json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hello" } }),
                ),
            ),
            frame(
                "content_block_stop",
                &j(&json!({ "type": "content_block_stop", "index": 0 })),
            ),
            // message_delta with stop_reason but NO usage object.
            frame(
                "message_delta",
                &j(&json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" } })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];

        let run = run_fixture(frames).await;
        assert_eq!(run.result.stop_reason, StopReason::Stop);
        assert!(run.result.error_message.is_none());
        let text = match &run.result.content[0] {
            Content::Text(t) => t,
            _ => panic!("text block"),
        };
        assert_eq!(text.text, "Hello");
        assert_eq!(run.result.usage.input, 12);
        assert_eq!(run.result.usage.total_tokens, 12);
    }

    // Mirrors `ignores unknown SSE events after message_stop` — `done` and
    // `proxy.stats` frames are skipped (non-ANTHROPIC_MESSAGE_EVENTS), so the
    // message still ends cleanly with Stop.
    #[tokio::test]
    async fn ignores_unknown_events_after_message_stop() {
        let mut frames = vec![
            frame(
                "message_start",
                &j(
                    &json!({ "type": "message_start", "message": { "id": "msg_test", "usage": { "input_tokens": 12, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } } }),
                ),
            ),
            frame(
                "content_block_start",
                &j(
                    &json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
                ),
            ),
            frame(
                "content_block_delta",
                &j(
                    &json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hello" } }),
                ),
            ),
            frame(
                "content_block_stop",
                &j(&json!({ "type": "content_block_stop", "index": 0 })),
            ),
            frame(
                "message_delta",
                &j(
                    &json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "input_tokens": 12, "output_tokens": 5, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } }),
                ),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];
        frames.push(frame("done", "[DONE]"));
        frames.push(frame("proxy.stats", "not json"));

        let run = run_fixture(frames).await;
        assert_eq!(run.result.stop_reason, StopReason::Stop);
        assert!(run.result.error_message.is_none());
        let text = match &run.result.content[0] {
            Content::Text(t) => t,
            _ => panic!("text block"),
        };
        assert_eq!(text.text, "Hello");
    }

    // ---- map_stop_reason table (anthropic-messages.ts:1326-1352) ----

    #[test]
    fn map_stop_reason_table() {
        let stop = |r: &str| map_stop_reason(r, None).unwrap();
        assert_eq!(stop("end_turn").stop_reason, StopReason::Stop);
        assert_eq!(stop("max_tokens").stop_reason, StopReason::Length);
        assert_eq!(stop("tool_use").stop_reason, StopReason::ToolUse);
        assert_eq!(stop("pause_turn").stop_reason, StopReason::Stop);
        assert_eq!(stop("stop_sequence").stop_reason, StopReason::Stop);

        // refusal with explanation.
        let details = json!({ "type": "refusal", "explanation": "blocked" });
        let refusal = map_stop_reason("refusal", Some(&details)).unwrap();
        assert_eq!(refusal.stop_reason, StopReason::Error);
        assert_eq!(refusal.error_message.as_deref(), Some("blocked"));

        // refusal without explanation → default message.
        let refusal_default = map_stop_reason("refusal", None).unwrap();
        assert_eq!(
            refusal_default.error_message.as_deref(),
            Some("The model refused to complete the request")
        );

        // sensitive → fixed message.
        let sensitive = map_stop_reason("sensitive", None).unwrap();
        assert_eq!(sensitive.stop_reason, StopReason::Error);
        assert_eq!(
            sensitive.error_message.as_deref(),
            Some("Provider stopped with: sensitive")
        );

        // unknown → error (TS throws).
        assert!(map_stop_reason("nonsense", None).is_err());
    }

    // `message_start` records the response id + initial usage (including the
    // 1h cache-write subset), mirroring anthropic-messages.ts:574-586.
    #[tokio::test]
    async fn message_start_records_response_id_and_usage() {
        let frames = vec![
            frame(
                "message_start",
                &j(&json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_abc",
                        "usage": {
                            "input_tokens": 10,
                            "output_tokens": 2,
                            "cache_read_input_tokens": 3,
                            "cache_creation_input_tokens": 4,
                            "cache_creation": { "ephemeral_1h_input_tokens": 1 },
                        },
                    },
                })),
            ),
            frame(
                "message_delta",
                &j(&json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" } })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];
        let run = run_fixture(frames).await;
        assert_eq!(run.result.response_id.as_deref(), Some("msg_abc"));
        assert_eq!(run.result.usage.input, 10);
        assert_eq!(run.result.usage.output, 2);
        assert_eq!(run.result.usage.cache_read, 3);
        assert_eq!(run.result.usage.cache_write, 4);
        assert_eq!(run.result.usage.cache_write_1h, Some(1));
        assert_eq!(run.result.usage.total_tokens, 10 + 2 + 3 + 4);
    }

    // `message_delta` usage's `output_tokens_details.thinking_tokens` is
    // recorded as `usage.reasoning` (a subset of output_tokens).
    #[tokio::test]
    async fn message_delta_records_reasoning_tokens() {
        let frames = vec![
            frame(
                "message_start",
                &j(
                    &json!({ "type": "message_start", "message": { "id": "m", "usage": { "input_tokens": 1, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } } }),
                ),
            ),
            frame(
                "message_delta",
                &j(&json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn" },
                    "usage": {
                        "input_tokens": 1,
                        "output_tokens": 50,
                        "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 0,
                        "output_tokens_details": { "thinking_tokens": 30 },
                    },
                })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];
        let run = run_fixture(frames).await;
        assert_eq!(run.result.usage.output, 50);
        assert_eq!(run.result.usage.reasoning, Some(30));
    }

    // `redacted_thinking` content_block_start produces a Thinking block with
    // the fixed marker text, the opaque payload in `thinking_signature`, and
    // `redacted: true`.
    #[tokio::test]
    async fn redacted_thinking_block() {
        let frames = vec![
            frame(
                "message_start",
                &j(
                    &json!({ "type": "message_start", "message": { "id": "m", "usage": { "input_tokens": 1, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } } }),
                ),
            ),
            frame(
                "content_block_start",
                &j(
                    &json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "redacted_thinking", "data": "opaque-base64" } }),
                ),
            ),
            frame(
                "content_block_stop",
                &j(&json!({ "type": "content_block_stop", "index": 0 })),
            ),
            frame(
                "message_delta",
                &j(&json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" } })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];
        let run = run_fixture(frames).await;
        let thinking = match &run.result.content[0] {
            Content::Thinking(t) => t,
            _ => panic!("thinking block"),
        };
        assert!(thinking.redacted);
        assert_eq!(thinking.thinking, "[Reasoning redacted]");
        assert_eq!(
            thinking.thinking_signature.as_deref(),
            Some("opaque-base64")
        );
        // Redacted thinking emits a thinking_start/thinking_end pair.
        assert!(run.tags.contains(&"thinking_start"));
        assert!(run.tags.contains(&"thinking_end"));
    }

    // A stream that reaches the protocol's terminal `message_stop` without a
    // `message_delta` (no stop_reason) finalizes as a NORMAL stop — proxy
    // gateways may omit `message_delta` (documented divergence from the TS
    // throw; `message_stop` is authoritative). The Pending-error is preserved
    // for streams that end WITHOUT `message_stop` (early SSE close).
    #[tokio::test]
    async fn message_stop_without_stop_reason_is_normal_stop() {
        let frames = vec![
            frame(
                "message_start",
                &j(
                    &json!({ "type": "message_start", "message": { "id": "m", "usage": { "input_tokens": 1, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } } }),
                ),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];
        let run = run_fixture(frames).await;
        assert_eq!(run.result.stop_reason, StopReason::Stop);
        assert_eq!(run.tags.last().copied(), Some("done"));
    }

    // A stream whose SSE closes BEFORE `message_stop` (no stop_reason, no
    // terminal event) still finalizes to an Error terminal — the early-close
    // case the Pending check guards.
    #[tokio::test]
    async fn stream_ending_before_message_stop_is_error() {
        let frames = vec![frame(
            "message_start",
            &j(
                &json!({ "type": "message_start", "message": { "id": "m", "usage": { "input_tokens": 1, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } } }),
            ),
        )];
        let run = run_fixture(frames).await;
        assert_eq!(run.result.stop_reason, StopReason::Error);
        assert_eq!(run.tags.last().copied(), Some("error"));
    }

    // `input_json_delta` re-parses the partial JSON on every delta so the
    // partial assistant message renders args live (invariant §5.15), and the
    // final `content_block_stop` does the authoritative parse.
    #[tokio::test]
    async fn tool_call_partial_json_reparse_each_delta() {
        let frames = vec![
            frame(
                "message_start",
                &j(
                    &json!({ "type": "message_start", "message": { "id": "m", "usage": { "input_tokens": 1, "output_tokens": 0, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0 } } }),
                ),
            ),
            frame(
                "content_block_start",
                &j(
                    &json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "t1", "name": "write", "input": {} } }),
                ),
            ),
            // Two deltas building the args object incrementally.
            frame(
                "content_block_delta",
                &j(
                    &json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"path\":\"a\"," } }),
                ),
            ),
            frame(
                "content_block_delta",
                &j(
                    &json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "\"text\":\"b\"}" } }),
                ),
            ),
            frame(
                "content_block_stop",
                &j(&json!({ "type": "content_block_stop", "index": 0 })),
            ),
            frame(
                "message_delta",
                &j(&json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" } })),
            ),
            frame("message_stop", &j(&json!({ "type": "message_stop" }))),
        ];
        let run = run_fixture(frames).await;
        let tc = match &run.result.content[0] {
            Content::ToolCall(t) => t,
            _ => panic!("toolcall"),
        };
        assert_eq!(tc.arguments, json!({ "path": "a", "text": "b" }));
        // Two input_json_delta frames → two toolcall_delta events + start + end.
        let deltas: Vec<&'static str> = run
            .tags
            .iter()
            .filter(|&&t| t == "toolcall_delta")
            .copied()
            .collect();
        assert_eq!(deltas.len(), 2);
    }

    // A stream that surfaces an `error` SSE event terminates with an Error
    // event carrying the SSE data as the message (parse_anthropic_event
    // rejects `event: error`).
    #[tokio::test]
    async fn sse_error_event_surfaces_error() {
        let (prod, stream) = create_assistant_message_event_stream();
        let mut prod = prod;
        let mut state =
            MapperState::new(Api::AnthropicMessages, "anthropic", "claude-haiku-4-5", 0);
        // An error frame — parse_anthropic_event returns Err.
        let err_frame = frame("error", "rate limited");
        match parse_anthropic_event(&err_frame) {
            Err(e) => emit_terminal_error(&mut prod, &mut state, e.to_string(), false).await,
            Ok(_) => panic!("expected error"),
        }
        let result = stream.result().await.expect("terminal");
        assert_eq!(result.stop_reason, StopReason::Error);
        assert!(result
            .error_message
            .as_deref()
            .unwrap()
            .contains("rate limited"));
    }
}
