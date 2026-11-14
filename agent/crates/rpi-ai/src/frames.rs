//! Assistant message frames — a compact, durable encoding of one assistant
//! stream.
//!
//! Mirrors `packages/ai/src/utils/assistant-message-frame.ts` (the type, the
//! `AssistantMessageFrameEncoder`, and `reduceAssistantMessageFrames`).
//!
//! # Why frames exist
//!
//! A persisted session normally holds *settled* messages. Frames are the
//! **progress** encoding: one frame per streaming step, small enough to append
//! to durable storage as the stream advances. If the process dies mid-stream,
//! the committed frame prefix can be replayed into the partial assistant
//! message that had been produced so far instead of losing the whole run —
//! see `docs/debugging/llm-repetition-forensics.md` §十一 and the harness-side recovery
//! that consumes these.
//!
//! # The encoder is not a pure mapping
//!
//! `AssistantMessageEvent::*Delta` carries `partial`, a *live accumulator*: the
//! accumulated block content may already be ahead of the event being encoded
//! (the provider mutates the message as it streams, and a queued event can be
//! consumed after more deltas landed). A naive `delta → frame` mapping would
//! replay those deltas twice. The encoder therefore tracks per-block offsets:
//!
//! - `covered_chars` — how much of the block the `*_start` snapshot already
//!   contained, so deltas beyond it are the only ones emitted.
//! - `delta_chars` — how many delta characters have been seen.
//! - for tool calls, `catchup_json` + a `toolcall_checkpoint`: the arguments in
//!   the `toolcall_start` snapshot may be *parsed* (across the whole partial
//!   JSON) while the raw delta stream has not caught up yet, so the raw
//!   catch-up buffer is emitted as a checkpoint instead of pretending the
//!   deltas captured it.
//!
//! The reducer (`reduce_frames`) is the inverse and the oracle: for an ordered,
//! complete frame stream it reproduces the message the provider settled on.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::json_parse::parse_streaming_json;
use crate::types::{
    AssistantMessage, AssistantMessageEvent, Content, StopReason, TextContent, ThinkingContent,
    ToolCall,
};

/// One step of a streamed assistant message. Field names match the reference
/// implementation's JSON so a frame list written by either side is readable by
/// the other.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantMessageFrame {
    /// The stream began. `partial` carries the message *identity* (api,
    /// provider, model, usage, timestamp); its content is empty by convention —
    /// blocks arrive through the `*_start` frames below.
    Start { partial: AssistantMessage },
    TextStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        content: TextContent,
    },
    TextDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    TextEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        content: String,
        #[serde(rename = "textSignature", skip_serializing_if = "Option::is_none")]
        text_signature: Option<String>,
    },
    ThinkingStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        content: ThinkingContent,
    },
    ThinkingDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        content: String,
        #[serde(rename = "thinkingSignature", skip_serializing_if = "Option::is_none")]
        thinking_signature: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        redacted: Option<bool>,
    },
    ToolcallStart {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        #[serde(rename = "toolCall")]
        tool_call: ToolCall,
    },
    /// Raw tool-call JSON that the `toolcall_start` snapshot already implied but
    /// the delta stream had not yet produced. See the module docs.
    ToolcallCheckpoint {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        json: String,
    },
    ToolcallDelta {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        delta: String,
    },
    ToolcallEnd {
        #[serde(rename = "contentIndex")]
        content_index: usize,
        id: String,
        name: String,
        arguments: Value,
        #[serde(rename = "thoughtSignature", skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "crate::types::deserialize_present_json"
        )]
        recorded: Option<Value>,
    },
}

impl AssistantMessageFrame {
    /// The frame's tag, for diagnostics.
    pub fn type_tag(&self) -> &'static str {
        match self {
            Self::Start { .. } => "start",
            Self::TextStart { .. } => "text_start",
            Self::TextDelta { .. } => "text_delta",
            Self::TextEnd { .. } => "text_end",
            Self::ThinkingStart { .. } => "thinking_start",
            Self::ThinkingDelta { .. } => "thinking_delta",
            Self::ThinkingEnd { .. } => "thinking_end",
            Self::ToolcallStart { .. } => "toolcall_start",
            Self::ToolcallCheckpoint { .. } => "toolcall_checkpoint",
            Self::ToolcallDelta { .. } => "toolcall_delta",
            Self::ToolcallEnd { .. } => "toolcall_end",
        }
    }
}

/// A frame stream that violates the encoder/reducer contract. The reference
/// implementation throws here; callers treat it as a broken stream rather than
/// corrupting the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameError(pub String);

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FrameError {}

fn frame_err(message: impl Into<String>) -> FrameError {
    FrameError(message.into())
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/// Per-block encoder bookkeeping.
enum EncoderBlockState {
    Text {
        covered_chars: usize,
        delta_chars: usize,
    },
    Thinking {
        covered_chars: usize,
        delta_chars: usize,
    },
    ToolCall {
        caught_up: bool,
        catchup_json: String,
        snapshot_arguments: String,
    },
}

impl EncoderBlockState {
    fn kind(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Thinking { .. } => "thinking",
            Self::ToolCall { .. } => "toolCall",
        }
    }
}

/// Mirrors `cloneStartMessage`: the start frame carries identity only; content
/// arrives through `*_start` frames.
fn clone_start_message(message: &AssistantMessage) -> AssistantMessage {
    let mut start = message.clone();
    start.content.clear();
    start.stop_reason = StopReason::Pending;
    start.error_message = None;
    start
}

fn serialized_arguments(arguments: &Value) -> String {
    serde_json::to_string(arguments).unwrap_or_else(|_| "null".to_string())
}

fn empty_parsed_tool_arguments() -> String {
    serialized_arguments(&parse_streaming_json(Some("")))
}

/// Whether `snapshot` can be extended into `current` (recursively): every
/// string in the snapshot is a prefix of the corresponding string in `current`.
fn is_json_prefix(snapshot: &Value, current: &Value) -> bool {
    match (snapshot, current) {
        (Value::String(snapshot), Value::String(current)) => current.starts_with(snapshot),
        (Value::Array(snapshot), Value::Array(current)) => {
            snapshot.len() <= current.len()
                && snapshot
                    .iter()
                    .zip(current.iter())
                    .all(|(a, b)| is_json_prefix(a, b))
        }
        (Value::Object(snapshot), Value::Object(current)) => snapshot.iter().all(|(key, value)| {
            current
                .get(key)
                .map(|current| is_json_prefix(value, current))
                .unwrap_or(false)
        }),
        // Scalars (and mismatched shapes) must be equal.
        (Value::Null, Value::Null) => true,
        (a, b) if !a.is_array() && !a.is_object() && !b.is_array() && !b.is_object() => a == b,
        _ => false,
    }
}

/// Encodes one assistant stream into frames. Mirrors
/// `AssistantMessageFrameEncoder`.
pub struct AssistantMessageFrameEncoder {
    started: bool,
    terminal: bool,
    blocks: BTreeMap<usize, EncoderBlockState>,
}

impl Default for AssistantMessageFrameEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AssistantMessageFrameEncoder {
    pub fn new() -> Self {
        Self {
            started: false,
            terminal: false,
            blocks: BTreeMap::new(),
        }
    }

    /// Whether the stream has been opened by a `Start` event.
    pub fn is_started(&self) -> bool {
        self.started
    }

    /// Encode one event. `Ok(None)` means "this event needs no frame" (terminal
    /// events, and deltas the `*_start` snapshot already covered).
    pub fn encode(
        &mut self,
        event: &AssistantMessageEvent,
    ) -> Result<Option<AssistantMessageFrame>, FrameError> {
        if self.terminal {
            return Err(frame_err(format!(
                "Assistant message event {} follows a terminal event",
                event.type_tag()
            )));
        }

        match event {
            AssistantMessageEvent::Start { partial } => {
                if self.started {
                    return Err(frame_err(
                        "Assistant message stream contains more than one start event",
                    ));
                }
                self.started = true;
                return Ok(Some(AssistantMessageFrame::Start {
                    partial: clone_start_message(partial),
                }));
            }
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                if !self.started && event.type_tag() == "done" {
                    return Err(frame_err(
                        "Assistant message done event appears before start",
                    ));
                }
                self.terminal = true;
                return Ok(None);
            }
            _ => {}
        }

        if !self.started {
            return Err(frame_err(format!(
                "Assistant message {} event appears before start",
                event.type_tag()
            )));
        }

        match event {
            AssistantMessageEvent::Start { .. }
            | AssistantMessageEvent::Done { .. }
            | AssistantMessageEvent::Error { .. } => unreachable!("handled above"),

            AssistantMessageEvent::TextStart {
                content_index,
                partial,
            } => {
                let content = text_block(partial, *content_index, "text_start")?;
                self.start_block(
                    *content_index,
                    EncoderBlockState::Text {
                        covered_chars: content.text.chars().count(),
                        delta_chars: 0,
                    },
                )?;
                Ok(Some(AssistantMessageFrame::TextStart {
                    content_index: *content_index,
                    content: content.clone(),
                }))
            }
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                ..
            } => self.encode_text_delta(*content_index, delta, "text"),
            AssistantMessageEvent::TextEnd {
                content_index,
                content,
                partial,
            } => {
                let block = text_block(partial, *content_index, "text_end")?;
                self.end_block(*content_index, "text")?;
                Ok(Some(AssistantMessageFrame::TextEnd {
                    content_index: *content_index,
                    content: content.clone(),
                    text_signature: block.text_signature.clone(),
                }))
            }

            AssistantMessageEvent::ThinkingStart {
                content_index,
                partial,
            } => {
                let content = thinking_block(partial, *content_index, "thinking_start")?;
                self.start_block(
                    *content_index,
                    EncoderBlockState::Thinking {
                        covered_chars: content.thinking.chars().count(),
                        delta_chars: 0,
                    },
                )?;
                Ok(Some(AssistantMessageFrame::ThinkingStart {
                    content_index: *content_index,
                    content: content.clone(),
                }))
            }
            AssistantMessageEvent::ThinkingDelta {
                content_index,
                delta,
                ..
            } => self.encode_text_delta(*content_index, delta, "thinking"),
            AssistantMessageEvent::ThinkingEnd {
                content_index,
                content,
                partial,
            } => {
                let block = thinking_block(partial, *content_index, "thinking_end")?;
                self.end_block(*content_index, "thinking")?;
                Ok(Some(AssistantMessageFrame::ThinkingEnd {
                    content_index: *content_index,
                    content: content.clone(),
                    thinking_signature: block.thinking_signature.clone(),
                    redacted: Some(block.redacted),
                }))
            }

            AssistantMessageEvent::ToolCallStart {
                content_index,
                partial,
            } => {
                let content = tool_call_block(partial, *content_index, "toolcall_start")?;
                let snapshot_arguments = serialized_arguments(&content.arguments);
                let caught_up = snapshot_arguments == empty_parsed_tool_arguments();
                self.start_block(
                    *content_index,
                    EncoderBlockState::ToolCall {
                        caught_up,
                        catchup_json: String::new(),
                        snapshot_arguments: if caught_up {
                            String::new()
                        } else {
                            snapshot_arguments
                        },
                    },
                )?;
                Ok(Some(AssistantMessageFrame::ToolcallStart {
                    content_index: *content_index,
                    tool_call: content.clone(),
                }))
            }
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                ..
            } => {
                let state = self.block_mut(*content_index, "toolCall")?;
                let EncoderBlockState::ToolCall {
                    caught_up,
                    catchup_json,
                    snapshot_arguments,
                } = state
                else {
                    return Err(frame_err("Unreachable tool-call encoder state"));
                };
                if *caught_up {
                    return Ok(if delta.is_empty() {
                        None
                    } else {
                        Some(AssistantMessageFrame::ToolcallDelta {
                            content_index: *content_index,
                            delta: delta.clone(),
                        })
                    });
                }
                catchup_json.push_str(delta);
                let arguments_value = parse_streaming_json(Some(catchup_json));
                if serialized_arguments(&arguments_value) != *snapshot_arguments {
                    // Legacy grammar calls include the initial input in
                    // `toolcall_start`, but their JSON delta stream still begins
                    // at an empty input. Its parsed arguments can therefore
                    // extend, rather than exactly reproduce, the start snapshot.
                    let snapshot = parse_streaming_json(Some(snapshot_arguments));
                    if !is_json_prefix(&snapshot, &arguments_value) {
                        return Ok(None);
                    }
                }
                *caught_up = true;
                snapshot_arguments.clear();
                let json = std::mem::take(catchup_json);
                Ok(if json.is_empty() {
                    None
                } else {
                    Some(AssistantMessageFrame::ToolcallCheckpoint {
                        content_index: *content_index,
                        json,
                    })
                })
            }
            AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call,
                partial,
            } => {
                let _ = tool_call_block(partial, *content_index, "toolcall_end")?;
                self.end_block(*content_index, "toolCall")?;
                Ok(Some(AssistantMessageFrame::ToolcallEnd {
                    content_index: *content_index,
                    id: tool_call.id.clone(),
                    name: tool_call.name.clone(),
                    arguments: tool_call.arguments.clone(),
                    thought_signature: tool_call.thought_signature.clone(),
                    namespace: tool_call.namespace.clone(),
                    recorded: tool_call.recorded.clone(),
                }))
            }
        }
    }

    fn start_block(
        &mut self,
        content_index: usize,
        state: EncoderBlockState,
    ) -> Result<(), FrameError> {
        if self.blocks.contains_key(&content_index) {
            return Err(frame_err(format!(
                "Assistant message block {content_index} starts more than once"
            )));
        }
        self.blocks.insert(content_index, state);
        Ok(())
    }

    fn block_mut(
        &mut self,
        content_index: usize,
        kind: &str,
    ) -> Result<&mut EncoderBlockState, FrameError> {
        let state = self.blocks.get_mut(&content_index).ok_or_else(|| {
            frame_err(format!(
                "Assistant message {kind} block {content_index} has not started"
            ))
        })?;
        if state.kind() != kind {
            return Err(frame_err(format!(
                "Assistant message block {content_index} is {}, not {kind}",
                state.kind()
            )));
        }
        Ok(state)
    }

    fn end_block(&mut self, content_index: usize, kind: &str) -> Result<(), FrameError> {
        self.block_mut(content_index, kind)?;
        self.blocks.remove(&content_index);
        Ok(())
    }

    fn encode_text_delta(
        &mut self,
        content_index: usize,
        delta: &str,
        kind: &'static str,
    ) -> Result<Option<AssistantMessageFrame>, FrameError> {
        let state = self.block_mut(content_index, kind)?;
        let (covered_chars, delta_chars) = match state {
            EncoderBlockState::Text {
                covered_chars,
                delta_chars,
            }
            | EncoderBlockState::Thinking {
                covered_chars,
                delta_chars,
            } => (*covered_chars, delta_chars),
            EncoderBlockState::ToolCall { .. } => {
                return Err(frame_err("Unreachable text encoder state"))
            }
        };
        let delta_start = *delta_chars;
        *delta_chars += delta.chars().count();
        // Portion of this delta that the `*_start` snapshot already contained.
        let covered = covered_chars.saturating_sub(delta_start);
        let delta_len = delta.chars().count();
        if covered >= delta_len {
            return Ok(None);
        }
        let remaining: String = delta.chars().skip(covered).collect();
        Ok(if remaining.is_empty() {
            None
        } else {
            Some(match kind {
                "thinking" => AssistantMessageFrame::ThinkingDelta {
                    content_index,
                    delta: remaining,
                },
                _ => AssistantMessageFrame::TextDelta {
                    content_index,
                    delta: remaining,
                },
            })
        })
    }
}

fn content_at<'a>(
    partial: &'a AssistantMessage,
    content_index: usize,
    frame_type: &str,
) -> Result<&'a Content, FrameError> {
    partial.content.get(content_index).ok_or_else(|| {
        frame_err(format!(
            "{frame_type} event has no content block at index {content_index}"
        ))
    })
}

fn text_block<'a>(
    partial: &'a AssistantMessage,
    content_index: usize,
    frame_type: &str,
) -> Result<&'a TextContent, FrameError> {
    match content_at(partial, content_index, frame_type)? {
        Content::Text(text) => Ok(text),
        other => Err(frame_err(format!(
            "{frame_type} event points to {} block at index {content_index}",
            content_kind(other)
        ))),
    }
}

fn thinking_block<'a>(
    partial: &'a AssistantMessage,
    content_index: usize,
    frame_type: &str,
) -> Result<&'a ThinkingContent, FrameError> {
    match content_at(partial, content_index, frame_type)? {
        Content::Thinking(thinking) => Ok(thinking),
        other => Err(frame_err(format!(
            "{frame_type} event points to {} block at index {content_index}",
            content_kind(other)
        ))),
    }
}

fn tool_call_block<'a>(
    partial: &'a AssistantMessage,
    content_index: usize,
    frame_type: &str,
) -> Result<&'a ToolCall, FrameError> {
    match content_at(partial, content_index, frame_type)? {
        Content::ToolCall(call) => Ok(call),
        other => Err(frame_err(format!(
            "{frame_type} event points to {} block at index {content_index}",
            content_kind(other)
        ))),
    }
}

fn content_kind(content: &Content) -> &'static str {
    match content {
        Content::Text(_) => "text",
        Content::Thinking(_) => "thinking",
        Content::ToolCall(_) => "toolCall",
        Content::Image(_) => "image",
    }
}

// ---------------------------------------------------------------------------
// Reducer
// ---------------------------------------------------------------------------

/// Reducer bookkeeping per block.
struct ReducerBlockState {
    kind: &'static str,
    ended: bool,
    /// Accumulated raw tool-call JSON (tool-call blocks only).
    json: String,
}

/// Replay a committed frame stream into the partial assistant message it
/// encodes. Returns `None` when the stream contains no `start` frame.
///
/// Mirrors `reduceAssistantMessageFrames`. The reducer never mutates the input
/// frames, so it is safe to call on frames read back from storage.
pub fn reduce_frames(
    frames: &[AssistantMessageFrame],
) -> Result<Option<AssistantMessage>, FrameError> {
    let mut message: Option<AssistantMessage> = None;
    let mut frame_before_start: Option<&'static str> = None;
    let mut states: BTreeMap<usize, ReducerBlockState> = BTreeMap::new();

    for frame in frames {
        if let AssistantMessageFrame::Start { partial } = frame {
            if message.is_some() {
                return Err(frame_err(
                    "Assistant message frame sequence contains more than one start frame",
                ));
            }
            if let Some(before) = frame_before_start {
                return Err(frame_err(format!(
                    "{before} frame appears before the start frame"
                )));
            }
            message = Some(clone_start_message(partial));
            continue;
        }
        let Some(message) = message.as_mut() else {
            if frame_before_start.is_none() {
                frame_before_start = Some(frame.type_tag());
            }
            continue;
        };

        match frame {
            AssistantMessageFrame::Start { .. } => unreachable!("handled above"),

            AssistantMessageFrame::TextStart {
                content_index,
                content,
            } => {
                append_block(
                    message,
                    &mut states,
                    *content_index,
                    Content::Text(content.clone()),
                    "text",
                )?;
            }
            AssistantMessageFrame::TextDelta {
                content_index,
                delta,
            } => {
                let (block, _) =
                    active_block(message, &mut states, *content_index, "text", "text_delta")?;
                if let Content::Text(text) = block {
                    text.text.push_str(delta);
                }
            }
            AssistantMessageFrame::TextEnd {
                content_index,
                content,
                text_signature,
            } => {
                let (block, state) =
                    active_block(message, &mut states, *content_index, "text", "text_end")?;
                if let Content::Text(text) = block {
                    text.text = content.clone();
                    text.text_signature = text_signature.clone();
                }
                state.ended = true;
            }

            AssistantMessageFrame::ThinkingStart {
                content_index,
                content,
            } => {
                append_block(
                    message,
                    &mut states,
                    *content_index,
                    Content::Thinking(content.clone()),
                    "thinking",
                )?;
            }
            AssistantMessageFrame::ThinkingDelta {
                content_index,
                delta,
            } => {
                let (block, _) = active_block(
                    message,
                    &mut states,
                    *content_index,
                    "thinking",
                    "thinking_delta",
                )?;
                if let Content::Thinking(thinking) = block {
                    thinking.thinking.push_str(delta);
                }
            }
            AssistantMessageFrame::ThinkingEnd {
                content_index,
                content,
                thinking_signature,
                redacted,
            } => {
                let (block, state) = active_block(
                    message,
                    &mut states,
                    *content_index,
                    "thinking",
                    "thinking_end",
                )?;
                if let Content::Thinking(thinking) = block {
                    thinking.thinking = content.clone();
                    thinking.thinking_signature = thinking_signature.clone();
                    thinking.redacted = redacted.unwrap_or(false);
                }
                state.ended = true;
            }

            AssistantMessageFrame::ToolcallStart {
                content_index,
                tool_call,
            } => {
                append_block(
                    message,
                    &mut states,
                    *content_index,
                    Content::ToolCall(tool_call.clone()),
                    "toolCall",
                )?;
            }
            AssistantMessageFrame::ToolcallCheckpoint {
                content_index,
                json,
            } => {
                let (block, state) = active_block(
                    message,
                    &mut states,
                    *content_index,
                    "toolCall",
                    "toolcall_checkpoint",
                )?;
                state.json = json.clone();
                if let Content::ToolCall(call) = block {
                    call.arguments = parse_streaming_json(Some(json));
                }
            }
            AssistantMessageFrame::ToolcallDelta {
                content_index,
                delta,
            } => {
                let (_, state) = active_block(
                    message,
                    &mut states,
                    *content_index,
                    "toolCall",
                    "toolcall_delta",
                )?;
                state.json.push_str(delta);
            }
            AssistantMessageFrame::ToolcallEnd {
                content_index,
                id,
                name,
                arguments,
                thought_signature,
                namespace,
                recorded,
            } => {
                let (block, state) = active_block(
                    message,
                    &mut states,
                    *content_index,
                    "toolCall",
                    "toolcall_end",
                )?;
                if let Content::ToolCall(call) = block {
                    call.id = id.clone();
                    call.name = name.clone();
                    call.arguments = arguments.clone();
                    call.thought_signature = thought_signature.clone();
                    call.namespace = namespace.clone();
                    call.recorded = recorded.clone();
                }
                state.ended = true;
            }
        }
    }

    let Some(mut message) = message else {
        return Ok(None);
    };

    // Tool-call blocks that never received an `end` frame still carry usable
    // arguments in their raw JSON buffer — parse whatever committed.
    for (content_index, state) in &states {
        if state.kind != "toolCall" || state.ended || state.json.is_empty() {
            continue;
        }
        if let Some(Content::ToolCall(call)) = message.content.get_mut(*content_index) {
            call.arguments = parse_streaming_json(Some(&state.json));
        } else {
            return Err(frame_err("Unreachable tool-call frame state"));
        }
    }

    Ok(Some(message))
}

fn append_block(
    message: &mut AssistantMessage,
    states: &mut BTreeMap<usize, ReducerBlockState>,
    content_index: usize,
    block: Content,
    kind: &'static str,
) -> Result<(), FrameError> {
    if content_index != message.content.len() {
        let reason = if content_index < message.content.len() {
            "already exists"
        } else {
            "would leave a gap"
        };
        return Err(frame_err(format!(
            "Cannot start assistant message block at index {content_index}: {reason}"
        )));
    }
    message.content.push(block);
    states.insert(
        content_index,
        ReducerBlockState {
            kind,
            ended: false,
            json: String::new(),
        },
    );
    Ok(())
}

fn active_block<'a>(
    message: &'a mut AssistantMessage,
    states: &'a mut BTreeMap<usize, ReducerBlockState>,
    content_index: usize,
    expected_kind: &str,
    frame_type: &str,
) -> Result<(&'a mut Content, &'a mut ReducerBlockState), FrameError> {
    let state = states.get_mut(&content_index).ok_or_else(|| {
        frame_err(format!(
            "{frame_type} frame has no started block at index {content_index}"
        ))
    })?;
    if state.kind != expected_kind {
        return Err(frame_err(format!(
            "{frame_type} frame expected {expected_kind} block at index {content_index}, found {}",
            state.kind
        )));
    }
    if state.ended {
        return Err(frame_err(format!(
            "{frame_type} frame follows the end of block at index {content_index}"
        )));
    }
    let block = message.content.get_mut(content_index).ok_or_else(|| {
        frame_err(format!(
            "{frame_type} frame has no started block at index {content_index}"
        ))
    })?;
    if content_kind(block) != expected_kind {
        return Err(frame_err(format!(
            "{frame_type} frame expected {expected_kind} block at index {content_index}, found {}",
            content_kind(block)
        )));
    }
    Ok((block, state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Api, DoneReason, ErrorReason, TextContentType, ToolCallType};
    use std::sync::Arc;

    fn base() -> AssistantMessage {
        AssistantMessage::empty(Api::AnthropicMessages, "anthropic", "claude-test", 7)
    }

    fn text_content(text: &str) -> TextContent {
        TextContent {
            kind: TextContentType,
            text: text.to_string(),
            text_signature: None,
        }
    }

    fn tool_call(id: &str, name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            kind: ToolCallType,
            id: id.to_string(),
            name: name.to_string(),
            arguments,
            thought_signature: None,
            namespace: None,
            recorded: None,
        }
    }

    fn encode_all(events: &[AssistantMessageEvent]) -> Vec<AssistantMessageFrame> {
        let mut encoder = AssistantMessageFrameEncoder::new();
        let mut frames = Vec::new();
        for event in events {
            if let Some(frame) = encoder.encode(event).expect("event encodes") {
                frames.push(frame);
            }
        }
        frames
    }

    /// Text stream: the reduced frames must reproduce the settled text exactly.
    #[test]
    fn text_stream_round_trips() {
        let mut partial = base();
        partial.content.push(Content::text(""));
        let partial = Arc::new(partial);

        let events = vec![
            AssistantMessageEvent::Start {
                partial: partial.clone(),
            },
            AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: partial.clone(),
            },
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "Hello".into(),
                partial: partial.clone(),
            },
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: ", world".into(),
                partial: partial.clone(),
            },
            AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: "Hello, world".into(),
                partial: partial.clone(),
            },
            AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message: base(),
            },
        ];
        let frames = encode_all(&events);
        assert_eq!(
            frames.iter().map(|f| f.type_tag()).collect::<Vec<_>>(),
            vec![
                "start",
                "text_start",
                "text_delta",
                "text_delta",
                "text_end"
            ]
        );
        let reduced = reduce_frames(&frames).unwrap().expect("a message");
        assert_eq!(
            reduced.content,
            vec![Content::text("Hello, world")],
            "reduced content must match what streamed"
        );
        assert_eq!(reduced.stop_reason, StopReason::Pending);
    }

    /// The crash case: a stream cut off mid-way must still reduce to the partial
    /// content that had been committed. This is the whole point of frames.
    #[test]
    fn truncated_stream_reduces_to_the_committed_partial() {
        let mut partial = base();
        partial.content.push(Content::text(""));
        let partial = Arc::new(partial);

        let frames = encode_all(&[
            AssistantMessageEvent::Start {
                partial: partial.clone(),
            },
            AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: partial.clone(),
            },
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "half a sen".into(),
                partial: partial.clone(),
            },
        ]);
        // No TextEnd, no Done — exactly what a kill leaves behind.
        let reduced = reduce_frames(&frames).unwrap().expect("a message");
        assert_eq!(reduced.content, vec![Content::text("half a sen")]);
    }

    #[test]
    fn thinking_then_text_round_trips() {
        let mut partial = base();
        partial.content.push(Content::thinking(""));
        partial.content.push(Content::text(""));
        let partial = Arc::new(partial);

        let frames = encode_all(&[
            AssistantMessageEvent::Start {
                partial: partial.clone(),
            },
            AssistantMessageEvent::ThinkingStart {
                content_index: 0,
                partial: partial.clone(),
            },
            AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: "weighing".into(),
                partial: partial.clone(),
            },
            AssistantMessageEvent::ThinkingEnd {
                content_index: 0,
                content: "weighing it".into(),
                partial: partial.clone(),
            },
            AssistantMessageEvent::TextStart {
                content_index: 1,
                partial: partial.clone(),
            },
            AssistantMessageEvent::TextDelta {
                content_index: 1,
                delta: "Answer".into(),
                partial: partial.clone(),
            },
            AssistantMessageEvent::TextEnd {
                content_index: 1,
                content: "Answer".into(),
                partial: partial.clone(),
            },
        ]);
        let reduced = reduce_frames(&frames).unwrap().expect("a message");
        assert_eq!(reduced.content.len(), 2, "{:?}", reduced.content);
        assert!(matches!(&reduced.content[0], Content::Thinking(t) if t.thinking == "weighing it"));
        assert!(matches!(&reduced.content[1], Content::Text(t) if t.text == "Answer"));
    }

    /// A `toolcall_end` carries the authoritative arguments; the raw JSON delta
    /// stream is the fallback when the stream never ended.
    #[test]
    fn tool_call_round_trips_both_ended_and_truncated() {
        let mut partial = base();
        partial
            .content
            .push(Content::ToolCall(tool_call("call-1", "read", sql_empty())));
        let partial = Arc::new(partial);

        let ended = encode_all(&[
            AssistantMessageEvent::Start {
                partial: partial.clone(),
            },
            AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                partial: partial.clone(),
            },
            AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: "{\"path\":\"a".into(),
                partial: partial.clone(),
            },
            AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: ".rs\"}".into(),
                partial: partial.clone(),
            },
            AssistantMessageEvent::ToolCallEnd {
                content_index: 0,
                tool_call: tool_call("call-1", "read", serde_json::json!({"path": "a.rs"})),
                partial: partial.clone(),
            },
        ]);
        let reduced = reduce_frames(&ended).unwrap().expect("a message");
        assert!(
            matches!(&reduced.content[0], Content::ToolCall(c)
                if c.name == "read" && c.arguments["path"] == "a.rs"),
            "{:?}",
            reduced.content
        );

        // Truncated: no `toolcall_end`, so the reducer must parse the raw JSON
        // buffer it did commit.
        let truncated = encode_all(&[
            AssistantMessageEvent::Start {
                partial: partial.clone(),
            },
            AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                partial: partial.clone(),
            },
            AssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: "{\"path\":\"a.rs\"}".into(),
                partial: partial.clone(),
            },
        ]);
        let reduced = reduce_frames(&truncated).unwrap().expect("a message");
        assert!(
            matches!(&reduced.content[0], Content::ToolCall(c) if c.arguments["path"] == "a.rs"),
            "un-ended tool call must still expose its committed arguments: {:?}",
            reduced.content
        );
    }

    #[test]
    fn frames_survive_a_json_round_trip() {
        // The whole point is durability: frames are appended to the session as
        // JSON, so the wire shape must round-trip.
        let mut partial = base();
        partial.content.push(Content::text(""));
        let frames = encode_all(&[
            AssistantMessageEvent::Start {
                partial: Arc::new(partial.clone()),
            },
            AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: Arc::new(partial.clone()),
            },
            AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "x".into(),
                partial: Arc::new(partial.clone()),
            },
        ]);
        let json = serde_json::to_string(&frames).expect("frames serialize");
        // Field names match the reference implementation.
        assert!(json.contains("\"type\":\"text_delta\""), "{json}");
        assert!(json.contains("\"contentIndex\":0"), "{json}");
        let parsed: Vec<AssistantMessageFrame> = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed, frames);
    }

    #[test]
    fn encoder_rejects_events_out_of_order() {
        let mut encoder = AssistantMessageFrameEncoder::new();
        let mut partial = base();
        partial.content.push(Content::text(""));
        let partial = Arc::new(partial);

        // A delta before `start` is a broken stream.
        let err = encoder
            .encode(&AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "x".into(),
                partial: partial.clone(),
            })
            .expect_err("delta before start must fail");
        assert!(err.0.contains("before start"), "{err}");

        // A second `start` is broken too.
        let mut encoder = AssistantMessageFrameEncoder::new();
        encoder
            .encode(&AssistantMessageEvent::Start {
                partial: partial.clone(),
            })
            .unwrap();
        let err = encoder
            .encode(&AssistantMessageEvent::Start {
                partial: partial.clone(),
            })
            .expect_err("second start must fail");
        assert!(err.0.contains("more than one start"), "{err}");
    }

    #[test]
    fn reducer_ignores_frames_before_start_and_reports_a_late_start() {
        // No start at all: nothing to reduce.
        let frames = vec![AssistantMessageFrame::TextDelta {
            content_index: 0,
            delta: "orphan".into(),
        }];
        assert_eq!(reduce_frames(&frames).unwrap(), None);

        // An orphan delta followed by a start is a corrupt sequence.
        let mut partial = base();
        partial.content.push(Content::text(""));
        let mut frames = frames;
        frames.push(AssistantMessageFrame::Start {
            partial: partial.clone(),
        });
        let err = reduce_frames(&frames).expect_err("late start must fail");
        assert!(err.0.contains("before the start frame"), "{err}");
    }

    /// The `covered_chars` machinery: when the `*_start` snapshot already
    /// contains text, the deltas that produced it must not be emitted again.
    #[test]
    fn deltas_already_covered_by_the_start_snapshot_are_not_replayed() {
        let mut partial = base();
        partial.content.push(Content::text("already"));
        let partial = Arc::new(partial);

        let mut encoder = AssistantMessageFrameEncoder::new();
        encoder
            .encode(&AssistantMessageEvent::Start {
                partial: partial.clone(),
            })
            .unwrap();
        // `text_start` snapshots 7 chars; the first delta only re-describes what
        // the snapshot already had.
        encoder
            .encode(&AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: partial.clone(),
            })
            .unwrap();
        let covered = encoder
            .encode(&AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "alrea".into(),
                partial: partial.clone(),
            })
            .unwrap();
        assert_eq!(covered, None, "fully-covered delta must emit no frame");
        let partial_delta = encoder
            .encode(&AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "dy more".into(),
                partial: partial.clone(),
            })
            .unwrap();
        assert_eq!(
            partial_delta,
            Some(AssistantMessageFrame::TextDelta {
                content_index: 0,
                delta: " more".into(),
            }),
            "only the uncovered tail is emitted"
        );
    }

    #[test]
    fn reducer_rejects_a_corrupt_frame_sequence() {
        let mut partial = base();
        partial.content.push(Content::text(""));
        // A delta with no `text_start`.
        let frames = vec![
            AssistantMessageFrame::Start { partial },
            AssistantMessageFrame::TextDelta {
                content_index: 0,
                delta: "x".into(),
            },
        ];
        let err = reduce_frames(&frames).expect_err("unstarted block must fail");
        assert!(err.0.contains("no started block"), "{err}");
    }

    #[test]
    fn reducer_rejects_a_content_gap() {
        // Index 1 with nothing at index 0 would leave a hole.
        let frames = vec![
            AssistantMessageFrame::Start { partial: base() },
            AssistantMessageFrame::TextStart {
                content_index: 1,
                content: text_content(""),
            },
        ];
        let err = reduce_frames(&frames).expect_err("gap must fail");
        assert!(err.0.contains("would leave a gap"), "{err}");
    }

    #[test]
    fn terminal_events_produce_no_frame_and_close_the_encoder() {
        let mut encoder = AssistantMessageFrameEncoder::new();
        let mut partial = base();
        partial.content.push(Content::text(""));
        let partial = Arc::new(partial);
        encoder
            .encode(&AssistantMessageEvent::Start {
                partial: partial.clone(),
            })
            .unwrap();
        assert_eq!(
            encoder
                .encode(&AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: base(),
                })
                .unwrap(),
            None
        );
        // Anything after the terminal event is a broken stream.
        let err = encoder
            .encode(&AssistantMessageEvent::TextStart {
                content_index: 0,
                partial,
            })
            .expect_err("post-terminal event must fail");
        assert!(err.0.contains("follows a terminal event"), "{err}");
    }

    #[test]
    fn an_error_terminal_also_closes_the_encoder() {
        let mut encoder = AssistantMessageFrameEncoder::new();
        let mut partial = base();
        partial.content.push(Content::text(""));
        encoder
            .encode(&AssistantMessageEvent::Start {
                partial: Arc::new(partial),
            })
            .unwrap();
        assert_eq!(
            encoder
                .encode(&AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error: base(),
                })
                .unwrap(),
            None
        );
    }

    /// Signature fields must survive: they are what lets an interrupted partial
    /// be sent back to the provider.
    #[test]
    fn text_and_thinking_signatures_round_trip() {
        let mut partial = base();
        partial.content.push(Content::Thinking(ThinkingContent {
            kind: crate::types::ThinkingContentType,
            thinking: String::new(),
            thinking_signature: Some("sig-123".into()),
            redacted: false,
        }));
        let partial = Arc::new(partial);
        let frames = encode_all(&[
            AssistantMessageEvent::Start {
                partial: partial.clone(),
            },
            AssistantMessageEvent::ThinkingStart {
                content_index: 0,
                partial: partial.clone(),
            },
            AssistantMessageEvent::ThinkingEnd {
                content_index: 0,
                content: "thought".into(),
                partial: partial.clone(),
            },
        ]);
        let reduced = reduce_frames(&frames).unwrap().expect("a message");
        assert!(
            matches!(&reduced.content[0], Content::Thinking(t)
                if t.thinking == "thought" && t.thinking_signature.as_deref() == Some("sig-123")),
            "{:?}",
            reduced.content
        );
    }

    fn sql_empty() -> Value {
        serde_json::json!({})
    }
}
