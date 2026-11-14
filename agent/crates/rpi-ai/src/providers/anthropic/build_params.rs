//! Mirrors `packages/ai/src/api/anthropic-messages.ts::buildParams` +
//! `convertMessages` + `convertTools` + `normalizeToolCallId`, plus
//! `packages/ai/src/utils/deferred-tools.ts::splitDeferredTools` and
//! `packages/ai/src/api/transform-messages.ts::transformMessages` — the
//! request-shaping layer that turns a pi `Context` + `SimpleStreamOptions`
//! into the JSON body POSTed to `/v1/messages`.
//!
//! API-key and OAuth requests share serialization. Subscription OAuth adds the
//! Claude Code system identity while transport authentication stays in mod.rs.

use crate::model::{AnthropicMessagesCompat, Model, StreamingProtocolCompat};
use crate::provider::SimpleStreamOptions;
use crate::strict_schema::make_strict_json_schema;
use crate::types::{
    AssistantMessage, Content, ImageContent, InputModality, Message, ThinkingLevel, Tool,
    ToolResultMessage,
};
use serde::ser::{SerializeSeq, SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashSet};
use std::sync::OnceLock;
use std::time::Instant;

// Re-expose the cache-control wire types from `cache_control.rs` under the
// local alias the TS-flavored names below expect.
use crate::providers::anthropic::cache_control::{
    get_cache_control, CacheControlEphemeral, CacheControlOption,
};

/// The token budget always reserved for the answer when a thinking budget
/// shares the response ceiling. Mirrors `simple-options.ts::MIN_ANSWER_TOKENS`.
pub const MIN_ANSWER_TOKENS: u64 = 1024;

/// The fine-grained-tool-streaming Anthropic beta header applied when the
/// request carries tools but the model lacks eager tool-input streaming.
const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";

/// The interleaved-thinking Anthropic beta header applied for budget-based
/// thinking models (adaptive-thinking models have it built in).
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";

// ----------------------------------------------------------------------------
// Wire types — the Anthropic Messages-API request shape (a subset). Unnamed in
// the TS (the SDK's `MessageCreateParamsStreaming`); we model only the fields
// pi emits so the request body serializes exactly.
// ----------------------------------------------------------------------------

/// A request-body content block. Tagged on `type`, matching Anthropic's wire
/// format (`text` / `image` / `thinking` / `redacted_thinking` / `tool_use` /
/// `tool_result` / `tool_reference`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    Image {
        source: AnthropicImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControlEphemeral>,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<ToolResultContent>,
        is_error: bool,
    },
    ToolReference {
        tool_name: String,
    },
}

/// The `source` of an `image` block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicImageSource {
    #[serde(rename = "type")]
    pub kind: String, // always "base64"
    pub media_type: String,
    pub data: std::sync::Arc<str>,
}

/// Tool-result content: either a plain string (concatenated text) or an array
/// of text/image/reference blocks. Mirrors the Anthropic `content` field union.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolResultContent {
    Text(String),
    Blocks(Vec<AnthropicToolResultBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicToolResultBlock {
    Text {
        text: String,
    },
    Image {
        source: AnthropicImageSource,
    },
    /// A deferred-tool load reference. Mirrors Anthropic's `tool_reference`
    /// content-block (replaces the ordinary text content of a `tool_result`
    /// when the result is being used to announce an on-demand tool load).
    ToolReference {
        tool_name: String,
    },
}

/// A serialized Anthropic tool definition. Mirrors the shape `convertTools`
/// emits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicTool {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eager_input_streaming: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    pub input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControlEphemeral>,
}

/// The thinking config blob. Mirrors the TS union
/// `{type:"adaptive",display}` | `{type:"enabled",budget_tokens,display}` |
/// `{type:"disabled"}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicThinking {
    Adaptive { display: String },
    Enabled { budget_tokens: u64, display: String },
    Disabled,
}

/// A system-prompt text block (carrying optional cache control).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicSystemBlock {
    #[serde(rename = "type")]
    pub kind: String, // "text"
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControlEphemeral>,
}

/// A user/assistant turn in the serialized request. `content` is either a
/// plain string (user-only) or a list of content blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: AnthropicMessageContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnthropicMessageContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

/// The full request body POSTed to `/v1/messages` with `stream:true`.
///
/// Field ordering mirrors the TS `MessageCreateParamsStreaming` so the on-wire
/// JSON matches what the reference emits byte-for-byte where it matters for
/// caching (system + messages + tools ordering). Unknown fields are dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    pub max_tokens: u64,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<AnthropicSystemBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<AnthropicThinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<AnthropicMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMetadata {
    pub user_id: String,
}

#[derive(Serialize)]
struct CanonicalTextBlock<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
}

struct CanonicalTextMessages<'a> {
    messages: &'a [Message],
    cache_control: Option<&'a CacheControlEphemeral>,
}

impl Serialize for CanonicalTextMessages<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let last_emitted = self.cache_control.and_then(|_| {
            self.messages
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, message)| {
                    let emits = match message {
                        Message::User(user) => matches!(
                            &user.content,
                            crate::types::UserContent::Text(text) if !text.trim().is_empty()
                        ),
                        Message::Assistant(assistant) => assistant.content.iter().any(|content| {
                            matches!(
                                content,
                                Content::Text(text) if !text.text.trim().is_empty()
                            )
                        }),
                        Message::ToolResult(_) => false,
                    };
                    emits.then_some(index)
                })
        });
        let mut sequence = serializer.serialize_seq(None)?;
        for (index, message) in self.messages.iter().enumerate() {
            match message {
                Message::User(user) => {
                    let crate::types::UserContent::Text(text) = &user.content else {
                        unreachable!("canonical text history contains a non-text user message");
                    };
                    if !text.trim().is_empty() {
                        if last_emitted == Some(index) && self.cache_control.is_some() {
                            sequence.serialize_element(&CanonicalTextCachedUser {
                                role: "user",
                                text,
                                cache_control: self.cache_control.expect("checked above"),
                            })?;
                        } else {
                            sequence.serialize_element(&CanonicalTextUser {
                                role: "user",
                                content: text,
                            })?;
                        }
                    }
                }
                Message::Assistant(assistant) => {
                    if assistant.content.iter().any(
                        |content| matches!(content, Content::Text(text) if !text.text.trim().is_empty()),
                    ) {
                        sequence.serialize_element(&CanonicalTextAssistant(assistant))?;
                    }
                }
                Message::ToolResult(_) => {
                    unreachable!("canonical text history contains a tool result");
                }
            }
        }
        sequence.end()
    }
}

#[derive(Serialize)]
struct CanonicalTextUser<'a> {
    role: &'static str,
    content: &'a str,
}

struct CanonicalTextCachedUser<'a> {
    role: &'static str,
    text: &'a str,
    cache_control: &'a CacheControlEphemeral,
}

impl Serialize for CanonicalTextCachedUser<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("AnthropicMessage", 2)?;
        state.serialize_field("role", self.role)?;
        state.serialize_field(
            "content",
            &CanonicalTextCachedUserBlocks {
                text: self.text,
                cache_control: self.cache_control,
            },
        )?;
        state.end()
    }
}

struct CanonicalTextCachedUserBlocks<'a> {
    text: &'a str,
    cache_control: &'a CacheControlEphemeral,
}

impl Serialize for CanonicalTextCachedUserBlocks<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(1))?;
        sequence.serialize_element(&CanonicalTextCachedBlock {
            kind: "text",
            text: self.text,
            cache_control: self.cache_control,
        })?;
        sequence.end()
    }
}

#[derive(Serialize)]
struct CanonicalTextCachedBlock<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    text: &'a str,
    cache_control: &'a CacheControlEphemeral,
}

struct CanonicalTextAssistant<'a>(&'a AssistantMessage);

impl Serialize for CanonicalTextAssistant<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("AnthropicMessage", 2)?;
        state.serialize_field("role", "assistant")?;
        state.serialize_field("content", &CanonicalTextAssistantBlocks(&self.0.content))?;
        state.end()
    }
}

struct CanonicalTextAssistantBlocks<'a>(&'a [Content]);

impl Serialize for CanonicalTextAssistantBlocks<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(None)?;
        for content in self.0 {
            let Content::Text(text) = content else {
                unreachable!("canonical text history contains a non-text assistant block");
            };
            if !text.text.trim().is_empty() {
                sequence.serialize_element(&CanonicalTextBlock {
                    kind: "text",
                    text: &text.text,
                })?;
            }
        }
        sequence.end()
    }
}

/// Reserve enough space for the long text-history portion of a canonical body.
/// The estimate deliberately excludes tool/schema bytes; underestimation only
/// causes normal Vec growth and never changes the serialized output.
fn canonical_text_body_capacity(messages: &[Message]) -> usize {
    let text_bytes = messages.iter().fold(0usize, |total, message| {
        let message_bytes = match message {
            Message::User(user) => match &user.content {
                crate::types::UserContent::Text(text) => text.len(),
                crate::types::UserContent::Blocks(_) => 0,
            },
            Message::Assistant(assistant) => assistant
                .content
                .iter()
                .filter_map(|content| match content {
                    Content::Text(text) => Some(text.text.len()),
                    _ => None,
                })
                .sum(),
            Message::ToolResult(_) => 0,
        };
        total.saturating_add(message_bytes)
    });
    text_bytes
        .saturating_add(messages.len().saturating_mul(64))
        .saturating_add(4096)
}

struct CanonicalTextRequest<'a> {
    request: &'a AnthropicRequest,
    messages: &'a [Message],
    cache_control: Option<&'a CacheControlEphemeral>,
}

impl Serialize for CanonicalTextRequest<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let request = self.request;
        let mut state = serializer.serialize_struct("AnthropicRequest", 10)?;
        state.serialize_field("model", &request.model)?;
        state.serialize_field(
            "messages",
            &CanonicalTextMessages {
                messages: self.messages,
                cache_control: self.cache_control,
            },
        )?;
        state.serialize_field("max_tokens", &request.max_tokens)?;
        state.serialize_field("stream", &request.stream)?;
        if let Some(value) = &request.system {
            state.serialize_field("system", value)?;
        }
        if let Some(value) = &request.temperature {
            state.serialize_field("temperature", value)?;
        }
        if let Some(value) = &request.tools {
            state.serialize_field("tools", value)?;
        }
        if let Some(value) = &request.thinking {
            state.serialize_field("thinking", value)?;
        }
        if let Some(value) = &request.output_config {
            state.serialize_field("output_config", value)?;
        }
        if let Some(value) = &request.metadata {
            state.serialize_field("metadata", value)?;
        }
        state.end()
    }
}

/// Return the number of emitted wire messages when the history can use the
/// borrowed text serializer. This deliberately accepts only the simplest
/// canonical subset; all other histories use the existing converter.
fn canonical_text_message_count(messages: &[Message], model: &Model) -> Option<usize> {
    let mut count = 0;
    for message in messages {
        match message {
            Message::User(user) => {
                let crate::types::UserContent::Text(text) = &user.content else {
                    return None;
                };
                count += usize::from(!text.trim().is_empty());
            }
            Message::Assistant(assistant) => {
                if !is_same_model(assistant, model)
                    || matches!(
                        assistant.stop_reason,
                        crate::types::StopReason::Error | crate::types::StopReason::Aborted
                    )
                    || assistant
                        .content
                        .iter()
                        .any(|content| !matches!(content, Content::Text(_)))
                {
                    return None;
                }
                count += usize::from(
                    assistant
                        .content
                        .iter()
                        .any(|content| matches!(content, Content::Text(text) if !text.text.trim().is_empty())),
                );
            }
            Message::ToolResult(_) => return None,
        }
    }
    Some(count)
}

/// The split between immediately-served tools and transcript-loaded deferred
/// tools. Mirrors `splitDeferredTools`'s return shape.
#[derive(Debug, Clone, Default)]
pub struct ToolPlacement {
    pub immediate: Vec<Tool>,
    pub deferred: Vec<Tool>,
    /// Normalized names of the deferred tools (so `convert_tool_result` can
    /// decide which references to emit).
    pub deferred_names: BTreeSet<String>,
}

static BUILD_PARAMS_DIAGNOSTICS: OnceLock<bool> = OnceLock::new();
static BUILD_PARAMS_DIAGNOSTICS_DETAIL: OnceLock<bool> = OnceLock::new();

fn build_params_diagnostics_enabled() -> bool {
    *BUILD_PARAMS_DIAGNOSTICS.get_or_init(|| {
        std::env::var_os("RPI_PROVIDER_DIAGNOSTICS")
            .or_else(|| std::env::var_os("RPI_AGENT_DIAGNOSTICS"))
            .is_some_and(|value| value != "0")
    })
}

fn build_params_diagnostics_detail_enabled() -> bool {
    *BUILD_PARAMS_DIAGNOSTICS_DETAIL.get_or_init(|| {
        build_params_diagnostics_enabled()
            && std::env::var_os("RPI_PROVIDER_DIAGNOSTICS_DETAIL").is_some_and(|value| value != "0")
    })
}

fn elapsed_ns(started: Option<Instant>) -> u64 {
    started
        .map(|started| started.elapsed().as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or_default()
}

/// Optional phase timings for one request build. All fields remain zero unless
/// provider diagnostics are enabled, so the normal request path does not pay
/// for per-phase clocks.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildParamsDiagnostics {
    pub normalize_ns: u64,
    pub deferred_tools_ns: u64,
    pub convert_messages_ns: u64,
    pub convert_tools_ns: u64,
    pub context_tokens_ns: u64,
    pub preencoded_json_ns: u64,
    pub total_ns: u64,
}

// ----------------------------------------------------------------------------
// normalizeToolCallId — anthropic-messages.ts:1077-1079
// ----------------------------------------------------------------------------

/// Normalize a tool-call id to Anthropic's `^[a-zA-Z0-9_-]+$` (max 64 chars)
/// requirement. Mirrors TS `normalizeToolCallId`. OpenAI Responses ids can be
/// 450+ chars with `|` etc.; this replaces every disallowed char with `_` and
/// truncates to 64.
pub fn normalize_tool_call_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

// ----------------------------------------------------------------------------
// transformMessages — transform-messages.ts
// ----------------------------------------------------------------------------

/// Same-model predicate: an assistant message is "same model" when provider +
/// api + id all match the request model. Mirrors the TS `isSameModel` check.
fn is_same_model(msg: &AssistantMessage, model: &Model) -> bool {
    msg.provider == model.provider && msg.api == model.api && msg.model == model.id
}

/// Common histories already satisfy the transform contract. Inspect them by
/// reference and retain the full normalizer for migrations and interrupted runs.
fn request_messages<'a>(messages: &'a [Message], model: &Model) -> std::borrow::Cow<'a, [Message]> {
    let supports_images = model.input.contains(&InputModality::Image);
    let mut pending = std::collections::HashSet::new();
    let canonical = messages.iter().all(|message| match message {
        Message::User(user) => {
            pending.is_empty()
                && (supports_images
                    || !matches!(&user.content, crate::types::UserContent::Blocks(blocks)
                        if blocks.iter().any(|block| matches!(block, Content::Image(_)))))
        }
        Message::ToolResult(result) => {
            pending.remove(result.tool_call_id.as_str());
            supports_images
                || !result
                    .content
                    .iter()
                    .any(|block| matches!(block, Content::Image(_)))
        }
        Message::Assistant(assistant) => {
            if !pending.is_empty()
                || !is_same_model(assistant, model)
                || matches!(
                    assistant.stop_reason,
                    crate::types::StopReason::Error | crate::types::StopReason::Aborted
                )
            {
                return false;
            }
            assistant.content.iter().all(|block| match block {
                Content::Thinking(thinking) => {
                    thinking.redacted
                        || !thinking.thinking.trim().is_empty()
                        || thinking
                            .thinking_signature
                            .as_deref()
                            .is_some_and(|signature| !signature.trim().is_empty())
                }
                Content::ToolCall(call) => {
                    pending.insert(call.id.as_str());
                    true
                }
                _ => true,
            })
        }
    }) && pending.is_empty();
    if canonical {
        std::borrow::Cow::Borrowed(messages)
    } else {
        std::borrow::Cow::Owned(transform_messages(messages, model))
    }
}

/// Tracks whether a message history is already in the canonical form expected
/// by the Anthropic wire converter. This is deliberately kept separate from
/// `request_messages`: the checked converter updates it while producing wire
/// messages, so canonical histories do not need a second full scan.
struct CanonicalHistory<'model, 'message> {
    model: &'model Model,
    supports_images: bool,
    pending: HashSet<&'message str>,
    canonical: bool,
}

impl<'model, 'message> CanonicalHistory<'model, 'message> {
    fn new(model: &'model Model) -> Self {
        Self {
            model,
            supports_images: model.input.contains(&InputModality::Image),
            pending: HashSet::new(),
            canonical: true,
        }
    }

    fn observe_user(&mut self, user: &crate::types::UserMessage) {
        if self.canonical
            && (!self.pending.is_empty()
                || (!self.supports_images
                    && matches!(&user.content, crate::types::UserContent::Blocks(blocks)
                        if blocks.iter().any(|block| matches!(block, Content::Image(_))))))
        {
            self.canonical = false;
        }
    }

    fn observe_tool_result(&mut self, result: &ToolResultMessage) {
        if self.canonical {
            self.pending.remove(result.tool_call_id.as_str());
            if !self.supports_images
                && result
                    .content
                    .iter()
                    .any(|block| matches!(block, Content::Image(_)))
            {
                self.canonical = false;
            }
        }
    }

    fn observe_assistant(&mut self, assistant: &'message AssistantMessage) {
        if !self.canonical {
            return;
        }
        if !self.pending.is_empty()
            || !is_same_model(assistant, self.model)
            || matches!(
                assistant.stop_reason,
                crate::types::StopReason::Error | crate::types::StopReason::Aborted
            )
        {
            self.canonical = false;
            return;
        }
        for block in &assistant.content {
            match block {
                Content::Thinking(thinking)
                    if !thinking.redacted
                        && thinking.thinking.trim().is_empty()
                        && thinking
                            .thinking_signature
                            .as_deref()
                            .is_none_or(|signature| signature.trim().is_empty()) =>
                {
                    self.canonical = false;
                    return;
                }
                Content::ToolCall(call) => {
                    self.pending.insert(call.id.as_str());
                }
                _ => {}
            }
        }
    }

    fn is_canonical(&self) -> bool {
        self.canonical && self.pending.is_empty()
    }
}

/// Walk a `Context`'s messages and apply, in order, the transform-messages
/// pipeline: unsupported-image downgrade, assistant thinking-block
/// (redacted/empty/cross-model) handling, tool-call-id normalization with
/// synthetic tool results for orphans, and skipping of errored/aborted
/// assistant messages. Mirrors `transform-messages.ts::transformMessages`.
///
/// `normalize_tool_call_id` is the Anthropic normalizer; `model.input` drives
/// the image-downgrade branch. The out `Vec<Message>` is what `convert_messages`
/// then serializes.
pub fn transform_messages(messages: &[Message], model: &Model) -> Vec<Message> {
    let supports_images = model.input.contains(&InputModality::Image);
    let mut tool_call_id_map: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    // First pass: image downgrade + thinking/tool-call transforms (mutate
    // per-message content; collect tool-call id remappings).
    let mut transformed: Vec<Message> = Vec::with_capacity(messages.len());
    for msg in messages {
        match msg {
            Message::User(u) => {
                if supports_images {
                    transformed.push(Message::User(u.clone()));
                } else {
                    let downgraded = downgrade_user_images(u);
                    transformed.push(Message::User(downgraded));
                }
            }
            Message::ToolResult(tr) => {
                // Normalize toolCallId via the map built in the assistant pass.
                let mut tr = *tr.clone();
                if let Some(remapped) = tool_call_id_map.get(&tr.tool_call_id) {
                    if remapped != &tr.tool_call_id {
                        tr.tool_call_id = remapped.clone();
                    }
                }
                if !supports_images {
                    tr.content = downgrade_tool_result_images(&tr.content);
                }
                transformed.push(Message::ToolResult(Box::new(tr)));
            }
            Message::Assistant(a) => {
                let a = a.as_ref();
                let same = is_same_model(a, model);
                let mut new_content: Vec<Content> = Vec::new();
                for block in &a.content {
                    match block {
                        Content::Thinking(t) => {
                            if t.redacted {
                                if same {
                                    new_content.push(block.clone());
                                }
                                // cross-model: drop redacted thinking.
                                continue;
                            }
                            let has_sig = t
                                .thinking_signature
                                .as_deref()
                                .map(|s| !s.trim().is_empty())
                                .unwrap_or(false);
                            if t.thinking.trim().is_empty() && !has_sig {
                                // empty thinking → drop.
                                continue;
                            }
                            if same && has_sig {
                                new_content.push(block.clone());
                            } else if same {
                                // same model, no signature but non-empty thinking: keep as-is
                                // (TS keeps the thinking block when isSameModel alone).
                                new_content.push(block.clone());
                            } else {
                                // cross-model: convert to plain text (signature dropped).
                                new_content.push(Content::text(t.thinking.clone()));
                            }
                        }
                        Content::Text(t) => {
                            if same {
                                new_content.push(block.clone());
                            } else {
                                // cross-model: rebuild as plain text (drops text_signature).
                                new_content.push(Content::text(t.text.clone()));
                            }
                        }
                        Content::ToolCall(tc) => {
                            let mut new_tc = tc.clone();
                            if !same {
                                new_tc.thought_signature = None;
                                let normalized = normalize_tool_call_id(&tc.id);
                                if normalized != tc.id {
                                    tool_call_id_map.insert(tc.id.clone(), normalized.clone());
                                    new_tc.id = normalized;
                                }
                            }
                            new_content.push(Content::ToolCall(new_tc));
                        }
                        Content::Image(_) => {
                            // Assistant-side images are not expected; pass through.
                            new_content.push(block.clone());
                        }
                    }
                }
                let mut new_msg = a.clone();
                new_msg.content = new_content;
                transformed.push(Message::Assistant(Box::new(new_msg)));
            }
        }
    }

    // Second pass: insert synthetic tool results for orphaned tool calls; drop
    // errored/aborted assistant messages.
    let mut result: Vec<Message> = Vec::with_capacity(transformed.len());
    let mut pending_tool_calls: Vec<crate::types::ToolCall> = Vec::new();
    let mut existing_tool_result_ids: BTreeSet<String> = BTreeSet::new();

    let flush_pending = |pending: &mut Vec<crate::types::ToolCall>,
                         existing: &BTreeSet<String>,
                         out: &mut Vec<Message>| {
        for tc in pending.drain(..) {
            if !existing.contains(&tc.id) {
                out.push(Message::ToolResult(Box::new(ToolResultMessage {
                    role: crate::types::ToolResultRole,
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    content: vec![Content::text("No result provided")],
                    details: None,
                    usage: None,
                    added_tool_names: Vec::new(),
                    is_error: true,
                    timestamp: 0,
                })));
            }
        }
    };

    for msg in transformed {
        match &msg {
            Message::Assistant(a) => {
                flush_pending(
                    &mut pending_tool_calls,
                    &existing_tool_result_ids,
                    &mut result,
                );
                existing_tool_result_ids.clear();
                let a = a.as_ref();
                // Skip errored/aborted turns entirely — they're incomplete.
                if matches!(
                    a.stop_reason,
                    crate::types::StopReason::Error | crate::types::StopReason::Aborted
                ) {
                    continue;
                }
                let tcs: Vec<crate::types::ToolCall> = a
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        Content::ToolCall(tc) => Some(tc.clone()),
                        _ => None,
                    })
                    .collect();
                if !tcs.is_empty() {
                    pending_tool_calls = tcs;
                }
                result.push(msg);
            }
            Message::ToolResult(tr) => {
                existing_tool_result_ids.insert(tr.tool_call_id.clone());
                result.push(msg);
            }
            Message::User(_) => {
                flush_pending(
                    &mut pending_tool_calls,
                    &existing_tool_result_ids,
                    &mut result,
                );
                existing_tool_result_ids.clear();
                result.push(msg);
            }
        }
    }
    flush_pending(
        &mut pending_tool_calls,
        &existing_tool_result_ids,
        &mut result,
    );
    result
}

/// Replace consecutive unsupported images with a single placeholder text block
/// in a user message. Mirrors `replaceImagesWithPlaceholder`.
fn replace_images_with_placeholder(content: &[Content], placeholder: &str) -> Vec<Content> {
    let mut out = Vec::new();
    let mut previous_was_placeholder = false;
    for block in content {
        match block {
            Content::Image(_) => {
                if !previous_was_placeholder {
                    out.push(Content::text(placeholder));
                }
                previous_was_placeholder = true;
            }
            Content::Text(t) => {
                out.push(block.clone());
                previous_was_placeholder = t.text == placeholder;
            }
            _ => {
                out.push(block.clone());
                previous_was_placeholder = false;
            }
        }
    }
    out
}

fn downgrade_user_images(u: &crate::types::UserMessage) -> crate::types::UserMessage {
    let placeholder = "(image omitted: model does not support images)";
    match &u.content {
        crate::types::UserContent::Text(_) => u.clone(),
        crate::types::UserContent::Blocks(blocks) => {
            let replaced = replace_images_with_placeholder(blocks, placeholder);
            let mut new_u = u.clone();
            new_u.content = crate::types::UserContent::Blocks(replaced);
            new_u
        }
    }
}

fn downgrade_tool_result_images(content: &[Content]) -> Vec<Content> {
    let placeholder = "(tool image omitted: model does not support images)";
    replace_images_with_placeholder(content, placeholder)
}

// ----------------------------------------------------------------------------
// split_deferred_tools — deferred-tools.ts
// ----------------------------------------------------------------------------

/// Split `ctx.tools` into immediate + deferred sets. A tool is deferred when
/// it appears in a `toolResult`'s `added_tool_names` but was never invoked by a
/// preceding assistant tool call — i.e. the transcript asked the provider to
/// load it on demand. Mirrors `splitDeferredTools`. When `enabled` is false
/// (the model doesn't accept client-side `tool_reference` blocks) every tool is
/// immediate.
pub fn split_deferred_tools(
    messages: &[Message],
    tools: &[Tool],
    enabled: bool,
    normalize_name: &dyn Fn(&str) -> String,
) -> ToolPlacement {
    // With references disabled, zero or one tool cannot be deduplicated or
    // reordered by the normalized-name map. Preserve the normalizer callback
    // invocation for the single-tool case because callers may make it stateful.
    if !enabled && tools.len() <= 1 {
        if let Some(tool) = tools.first() {
            let _ = normalize_name(&tool.name);
        }
        return ToolPlacement {
            immediate: tools.to_vec(),
            deferred: Vec::new(),
            deferred_names: BTreeSet::new(),
        };
    }

    // Dedupe tools by normalized name (last wins, matching Map semantics).
    let mut unique: std::collections::BTreeMap<String, Tool> = std::collections::BTreeMap::new();
    for tool in tools {
        unique.insert(normalize_name(&tool.name), tool.clone());
    }
    if !enabled {
        return ToolPlacement {
            immediate: unique.into_values().collect(),
            deferred: Vec::new(),
            deferred_names: BTreeSet::new(),
        };
    }

    let mut deferred_names: BTreeSet<String> = BTreeSet::new();
    let mut used_names: BTreeSet<String> = BTreeSet::new();
    for msg in messages {
        match msg {
            Message::Assistant(a) => {
                for block in &a.content {
                    if let Content::ToolCall(tc) = block {
                        used_names.insert(normalize_name(&tc.name));
                    }
                }
            }
            Message::ToolResult(tr) => {
                for name in &tr.added_tool_names {
                    let n = normalize_name(name);
                    if !used_names.contains(&n) {
                        deferred_names.insert(n);
                    }
                }
            }
            _ => {}
        }
    }

    let mut immediate = Vec::new();
    let mut deferred = Vec::new();
    for (name, tool) in unique {
        if deferred_names.contains(&name) {
            deferred.push(tool);
        } else {
            immediate.push(tool);
        }
    }
    ToolPlacement {
        immediate,
        deferred,
        deferred_names,
    }
}

// ----------------------------------------------------------------------------
// convertTools — anthropic-messages.ts:1287-1324
// ----------------------------------------------------------------------------

/// Resolve whether a tool should request strict JSON-schema constrained
/// sampling. Mirrors `resolveJsonSchemaStrictSampling` (simplified for v1: only
/// `json_schema` configs, no grammar). Returns `Some(true)` only when the tool
/// opts in via `constrained_sampling` AND `supports_strict_tools` AND the
/// schema passes the strict-subset guard.
fn resolve_strict(tool: &Tool, supports_strict_tools: bool) -> Option<bool> {
    let config = tool.constrained_sampling.as_ref()?;
    if !matches!(
        config,
        crate::types::ConstrainedSamplingConfig::JsonSchema { .. }
    ) {
        return None;
    }
    if supports_strict_tools {
        // The TS attempts `makeStrictJsonSchema` and returns undefined on
        // UnsupportedStrictJsonSchemaError (unless strict=require). We attempt
        // the strict transform; on failure we fall back to undefined for
        // `prefer`, and surface an error for `require`.
        match make_strict_json_schema(tool.parameters.as_value()) {
            Ok(_) => Some(true),
            Err(_) => {
                if matches!(
                    config,
                    crate::types::ConstrainedSamplingConfig::JsonSchema {
                        strict: crate::types::ConstrainedStrictness::Require,
                    }
                ) {
                    // v1 simplification: downgrade to non-strict rather than
                    // panic the request; the agent loop surfaces coercion
                    // failures separately. TODO: propagate as a provider error.
                    None
                } else {
                    None
                }
            }
        }
    } else {
        None
    }
}

/// Build the `input_schema` for a tool. Mirrors `getJsonSchemaToolParameters`
/// + the `legacyInputSchema` merge in `convertTools`. Strict mode emits the
/// full strict schema (parameters merged over `{type:object, properties, required}`);
/// non-strict emits only the legacy `{type:object, properties, required}` shape.
fn tool_input_schema(tool: &Tool, strict: Option<bool>) -> Value {
    let params = tool.parameters.as_value();
    let properties = params
        .get("properties")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let required = params
        .get("required")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let mut legacy = json!({
        "type": "object",
        "properties": properties,
        "required": required,
    });
    // `properties` are copied verbatim, so any `$ref` they carry (e.g. the `edit`
    // tool's `#/definitions/ReplaceEditJson`) must keep its target: carry the
    // `definitions` / `$defs` sections too, or the schema is not self-contained
    // and a strict validator rejects it (DeepSeek's Anthropic endpoint returns
    // `Pointer '/definitions/X' does not exist`).
    if let Some(object) = legacy.as_object_mut() {
        for key in ["definitions", "$defs"] {
            if let Some(section) = params.get(key) {
                object.insert(key.to_string(), section.clone());
            }
        }
    }
    match strict {
        Some(true) => {
            // Strict: attempt the strict schema; on failure fall back to legacy.
            match make_strict_json_schema(params) {
                Ok(strict_schema) => {
                    // Merge strict schema over legacy (legacy's type/properties/required win).
                    merge_values(strict_schema, legacy)
                }
                Err(_) => legacy,
            }
        }
        _ => legacy,
    }
}

/// Convert a slice of pi tools to Anthropic wire tools. Mirrors `convertTools`.
/// `cache_control` is attached only to the LAST immediate tool when
/// `supports_cache_control_on_tools` and the caller passed a control block;
/// deferred tools never carry cache control. `defer_loading` marks deferred
/// tools; `eager_input_streaming` marks tools when the model supports it.
pub fn convert_tools(
    tools: &[Tool],
    supports_eager_tool_input_streaming: bool,
    supports_strict_tools: bool,
    cache_control_on_tools: bool,
    cache_control: Option<&CacheControlEphemeral>,
    defer_loading: bool,
) -> Vec<AnthropicTool> {
    let mut out = Vec::with_capacity(tools.len());
    let last_index = tools.len().saturating_sub(1);
    for (index, tool) in tools.iter().enumerate() {
        let strict = resolve_strict(tool, supports_strict_tools);
        let input_schema = tool_input_schema(tool, strict);
        // cache_control lands only on the last immediate tool (defer_loading=false)
        // when the model supports it and a control block was supplied.
        let cc = if !defer_loading
            && cache_control_on_tools
            && cache_control.is_some()
            && index == last_index
        {
            cache_control.cloned()
        } else {
            None
        };
        out.push(AnthropicTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
            eager_input_streaming: if supports_eager_tool_input_streaming {
                Some(true)
            } else {
                None
            },
            strict: if strict == Some(true) {
                Some(true)
            } else {
                None
            },
            input_schema,
            defer_loading: if defer_loading { Some(true) } else { None },
            cache_control: cc,
        });
    }
    out
}

// ----------------------------------------------------------------------------
// convertMessages — anthropic-messages.ts:1116-1281
// ----------------------------------------------------------------------------

/// Sanitize Unicode surrogates in a string so it round-trips through JSON
/// safely (lone surrogates are invalid UTF-8 / JSON). Mirrors `sanitizeSurrogates`.
/// Rust `String`s are already valid UTF-8 (no lone surrogates survive `String`
/// construction), so this is a no-op pass-through kept for parity.
fn sanitize_surrogates(s: &str) -> String {
    s.to_string()
}

/// Convert pi `Message`s to Anthropic `AnthropicMessage`s. Mirrors
/// `convertMessages`. `cache_control` is attached to the last block of the last
/// user message (after consecutive grouping of tool results) when present.
pub fn convert_messages(
    messages: &[Message],
    cache_control: Option<&CacheControlEphemeral>,
    allow_empty_signature: bool,
    deferred_tool_names: &BTreeSet<String>,
    normalize_name: &dyn Fn(&str) -> String,
) -> Vec<AnthropicMessage> {
    convert_messages_impl(
        messages,
        cache_control,
        allow_empty_signature,
        deferred_tool_names,
        normalize_name,
        None,
    )
}

/// Convert messages while checking canonicality in the same traversal. The
/// returned wire messages are usable only when `canonical` is true; callers
/// must fall back to `transform_messages` for non-canonical histories.
fn convert_messages_checked(
    messages: &[Message],
    cache_control: Option<&CacheControlEphemeral>,
    allow_empty_signature: bool,
    normalize_name: &dyn Fn(&str) -> String,
    model: &Model,
) -> (Vec<AnthropicMessage>, bool) {
    let mut history = CanonicalHistory::new(model);
    let wire = convert_messages_impl(
        messages,
        cache_control,
        allow_empty_signature,
        &BTreeSet::new(),
        normalize_name,
        Some(&mut history),
    );
    (wire, history.is_canonical())
}

fn convert_messages_impl<'message>(
    messages: &'message [Message],
    cache_control: Option<&CacheControlEphemeral>,
    allow_empty_signature: bool,
    deferred_tool_names: &BTreeSet<String>,
    normalize_name: &dyn Fn(&str) -> String,
    mut canonical: Option<&mut CanonicalHistory<'_, 'message>>,
) -> Vec<AnthropicMessage> {
    let mut params: Vec<AnthropicMessage> = Vec::with_capacity(messages.len());
    let mut loaded_tool_names: BTreeSet<String> = BTreeSet::new();
    let n = messages.len();
    let mut i = 0;
    while i < n {
        let msg = &messages[i];
        match msg {
            Message::User(u) => {
                if let Some(history) = canonical.as_deref_mut() {
                    history.observe_user(u);
                }
                match &u.content {
                    crate::types::UserContent::Text(s) => {
                        if !s.trim().is_empty() {
                            params.push(AnthropicMessage {
                                role: "user".into(),
                                content: AnthropicMessageContent::Text(sanitize_surrogates(s)),
                            });
                        }
                    }
                    crate::types::UserContent::Blocks(blocks) => {
                        let mut wire_blocks: Vec<AnthropicContentBlock> = Vec::new();
                        for b in blocks {
                            match b {
                                Content::Text(t) => {
                                    wire_blocks.push(AnthropicContentBlock::Text {
                                        text: sanitize_surrogates(&t.text),
                                        cache_control: None,
                                    });
                                }
                                Content::Image(img) => {
                                    wire_blocks.push(image_block(img));
                                }
                                _ => {}
                            }
                        }
                        // Whitespace-filter text blocks; drop the message if empty.
                        wire_blocks.retain(|b| match b {
                            AnthropicContentBlock::Text { text, .. } => !text.trim().is_empty(),
                            _ => true,
                        });
                        if wire_blocks.is_empty() {
                            i += 1;
                            continue;
                        }
                        params.push(AnthropicMessage {
                            role: "user".into(),
                            content: AnthropicMessageContent::Blocks(wire_blocks),
                        });
                    }
                }
                i += 1;
            }
            Message::Assistant(a) => {
                let a = a.as_ref();
                if let Some(history) = canonical.as_deref_mut() {
                    history.observe_assistant(a);
                }
                let mut wire_blocks: Vec<AnthropicContentBlock> = Vec::new();
                for block in &a.content {
                    match block {
                        Content::Text(t) => {
                            if t.text.trim().is_empty() {
                                continue;
                            }
                            wire_blocks.push(AnthropicContentBlock::Text {
                                text: sanitize_surrogates(&t.text),
                                cache_control: None,
                            });
                        }
                        Content::Thinking(t) => {
                            if t.redacted {
                                wire_blocks.push(AnthropicContentBlock::RedactedThinking {
                                    data: t.thinking_signature.clone().unwrap_or_default(),
                                });
                                continue;
                            }
                            let sig = t.thinking_signature.as_deref().unwrap_or("");
                            let has_sig = !sig.trim().is_empty();
                            if t.thinking.trim().is_empty() && !has_sig {
                                continue;
                            }
                            if !has_sig {
                                // Missing/empty signature: allow_empty_signature preserves
                                // the thinking block; otherwise downgrade to plain text.
                                if allow_empty_signature {
                                    wire_blocks.push(AnthropicContentBlock::Thinking {
                                        thinking: sanitize_surrogates(&t.thinking),
                                        signature: String::new(),
                                    });
                                } else {
                                    wire_blocks.push(AnthropicContentBlock::Text {
                                        text: sanitize_surrogates(&t.thinking),
                                        cache_control: None,
                                    });
                                }
                            } else {
                                wire_blocks.push(AnthropicContentBlock::Thinking {
                                    thinking: sanitize_surrogates(&t.thinking),
                                    signature: sig.to_string(),
                                });
                            }
                        }
                        Content::ToolCall(tc) => {
                            wire_blocks.push(AnthropicContentBlock::ToolUse {
                                id: tc.id.clone(),
                                name: tc.name.clone(),
                                input: tc.arguments.clone(),
                                cache_control: None,
                            });
                        }
                        Content::Image(_) => {
                            // Assistant-side images are not emitted by the protocol.
                        }
                    }
                }
                if !wire_blocks.is_empty() {
                    params.push(AnthropicMessage {
                        role: "assistant".into(),
                        content: AnthropicMessageContent::Blocks(wire_blocks),
                    });
                }
                i += 1;
            }
            Message::ToolResult(_) => {
                // Collect all consecutive toolResult messages; Anthropic wants them
                // grouped into a single user turn.
                let mut tool_results: Vec<AnthropicContentBlock> = Vec::new();
                let mut sibling_content: Vec<AnthropicContentBlock> = Vec::new();
                let mut j = i;
                while j < n {
                    let tr = match &messages[j] {
                        Message::ToolResult(tr) => tr.as_ref(),
                        _ => break,
                    };
                    if let Some(history) = canonical.as_deref_mut() {
                        history.observe_tool_result(tr);
                    }
                    let (tool_result, siblings) = convert_tool_result(
                        tr,
                        deferred_tool_names,
                        &mut loaded_tool_names,
                        normalize_name,
                    );
                    tool_results.push(tool_result);
                    sibling_content.extend(siblings);
                    j += 1;
                }
                i = j;
                let mut content = tool_results;
                content.extend(sibling_content);
                params.push(AnthropicMessage {
                    role: "user".into(),
                    content: AnthropicMessageContent::Blocks(content),
                });
            }
        }
    }

    // Attach cache_control to the last block of the last user message.
    if let Some(cc) = cache_control {
        if let Some(last) = params.last_mut() {
            if last.role == "user" {
                attach_cache_control_to_last_block(last, cc);
            }
        }
    }

    params
}

/// Build the `tool_result` + sibling `tool_reference` blocks for one
/// `ToolResultMessage`. Mirrors `convertToolResult`. When the result carries
/// `added_tool_names` that are deferred AND not yet loaded, the tool_result's
/// content is replaced with `tool_reference` blocks and the actual text/images are
/// emitted as sibling content (Anthropic rejects mixing references with
/// ordinary content in a single tool_result).
fn convert_tool_result(
    msg: &ToolResultMessage,
    deferred_tool_names: &BTreeSet<String>,
    loaded: &mut BTreeSet<String>,
    normalize_name: &dyn Fn(&str) -> String,
) -> (AnthropicContentBlock, Vec<AnthropicContentBlock>) {
    // Collect tool_reference entries for deferred tools not yet loaded. Mirrors
    // the TS `references` loop.
    let mut references: Vec<AnthropicToolResultBlock> = Vec::new();
    for name in &msg.added_tool_names {
        let n = normalize_name(name);
        if !deferred_tool_names.contains(&n) || loaded.contains(&n) {
            continue;
        }
        loaded.insert(n);
        references.push(AnthropicToolResultBlock::ToolReference {
            tool_name: name.clone(),
        });
    }
    let converted_content = convert_content_blocks(&msg.content);
    if !references.is_empty() {
        // Anthropic rejects tool references mixed with ordinary tool-result
        // content, so the references displace the real content into sibling
        // user text/image blocks that follow the tool_result.
        let siblings = sibling_content_from(converted_content);
        let tool_result = AnthropicContentBlock::ToolResult {
            tool_use_id: msg.tool_call_id.clone(),
            content: Some(ToolResultContent::Blocks(references)),
            is_error: msg.is_error,
        };
        (tool_result, siblings)
    } else {
        let tool_result = AnthropicContentBlock::ToolResult {
            tool_use_id: msg.tool_call_id.clone(),
            content: Some(converted_content),
            is_error: msg.is_error,
        };
        (tool_result, Vec::new())
    }
}

/// Turn a converted tool-result content into sibling user text/image blocks for the
/// reference-displacement path. Mirrors the TS `siblingContent` construction.
fn sibling_content_from(converted: ToolResultContent) -> Vec<AnthropicContentBlock> {
    match converted {
        ToolResultContent::Text(s) => vec![AnthropicContentBlock::Text {
            text: s,
            cache_control: None,
        }],
        ToolResultContent::Blocks(blocks) => blocks
            .into_iter()
            .map(|b| match b {
                AnthropicToolResultBlock::Text { text } => AnthropicContentBlock::Text {
                    text,
                    cache_control: None,
                },
                AnthropicToolResultBlock::Image { source } => AnthropicContentBlock::Image {
                    source,
                    cache_control: None,
                },
                AnthropicToolResultBlock::ToolReference { tool_name } => {
                    AnthropicContentBlock::ToolReference { tool_name }
                }
            })
            .collect(),
    }
}

/// Convert tool-result content blocks to Anthropic wire form. Mirrors
/// `convertContentBlocks`. Text-only results collapse to a single string;
/// image-bearing results emit explicit text+image blocks.
fn convert_content_blocks(content: &[Content]) -> ToolResultContent {
    let has_images = content.iter().any(|c| matches!(c, Content::Image(_)));
    if !has_images {
        let text: String = content
            .iter()
            .filter_map(|c| match c {
                Content::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return ToolResultContent::Text(sanitize_surrogates(&text));
    }
    let mut blocks: Vec<AnthropicToolResultBlock> = Vec::new();
    for block in content {
        match block {
            Content::Text(t) => {
                blocks.push(AnthropicToolResultBlock::Text {
                    text: sanitize_surrogates(&t.text),
                });
            }
            Content::Image(img) => {
                // Unsupported images were already downgraded by transform_messages.
                blocks.push(AnthropicToolResultBlock::Image {
                    source: AnthropicImageSource {
                        kind: "base64".into(),
                        media_type: img.mime_type.clone(),
                        data: img.data.clone(),
                    },
                });
            }
            _ => {}
        }
    }
    ToolResultContent::Blocks(blocks)
}

fn image_block(img: &ImageContent) -> AnthropicContentBlock {
    AnthropicContentBlock::Image {
        source: AnthropicImageSource {
            kind: "base64".into(),
            media_type: img.mime_type.clone(),
            data: img.data.clone(),
        },
        cache_control: None,
    }
}

/// Attach a `cache_control` blob to the last block of a user message. Mirrors
/// the TS post-loop `cacheControl` attachment (lines 1256-1278).
fn attach_cache_control_to_last_block(msg: &mut AnthropicMessage, cc: &CacheControlEphemeral) {
    match &mut msg.content {
        AnthropicMessageContent::Text(s) => {
            // String content becomes a single text block carrying cache_control.
            let text = std::mem::take(s);
            msg.content = AnthropicMessageContent::Blocks(vec![AnthropicContentBlock::Text {
                text,
                cache_control: Some(cc.clone()),
            }]);
        }
        AnthropicMessageContent::Blocks(blocks) => {
            if let Some(last) = blocks.last_mut() {
                set_cache_control_on_block(last, Some(cc.clone()));
            }
        }
    }
}

fn set_cache_control_on_block(
    block: &mut AnthropicContentBlock,
    cc: Option<CacheControlEphemeral>,
) {
    match block {
        AnthropicContentBlock::Text { cache_control, .. }
        | AnthropicContentBlock::Image { cache_control, .. }
        | AnthropicContentBlock::ToolUse { cache_control, .. } => {
            *cache_control = cc;
        }
        _ => {}
    }
}

// ----------------------------------------------------------------------------
// Thinking config — anthropic-messages.ts:1027-1056 + simple-options.ts
// ----------------------------------------------------------------------------

/// Default per-level thinking budgets (tokens). minimal..high mirror the
/// `defaultBudgets` literal in `adjustMaxTokensForThinking`; xhigh and max
/// double upward from high.
pub fn default_thinking_budgets() -> crate::types::ThinkingBudgets {
    crate::types::ThinkingBudgets {
        minimal: Some(1024),
        low: Some(2048),
        medium: Some(8192),
        high: Some(16384),
        xhigh: Some(32768),
        max: Some(65536),
    }
}

/// The thinking budget for one level, custom budgets first. Mirrors
/// `thinkingBudgetForLevel`, except that `xhigh` and `max` keep their own
/// budgets on models whose `thinkingLevelMap` offers them; collapsing them onto
/// `high` made the extra levels identical to `high` on budget-based models.
/// Models that don't offer them still clamp to `high` (`clampReasoning`).
/// `0` when thinking is off.
pub fn thinking_budget_for_level(
    model: &Model,
    level: ThinkingLevel,
    custom_budgets: Option<&crate::types::ThinkingBudgets>,
) -> u64 {
    let defaults = default_thinking_budgets();
    let custom = custom_budgets.copied().unwrap_or_default();
    let level = match level {
        ThinkingLevel::Xhigh | ThinkingLevel::Max => model.clamp_thinking_level(level),
        other => other,
    };
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => custom.minimal.or(defaults.minimal),
        ThinkingLevel::Low => custom.low.or(defaults.low),
        ThinkingLevel::Medium => custom.medium.or(defaults.medium),
        ThinkingLevel::High => custom.high.or(defaults.high),
        ThinkingLevel::Xhigh => custom.xhigh.or(defaults.xhigh),
        ThinkingLevel::Max => custom.max.or(defaults.max),
    }
    .unwrap_or(0)
}

/// Fit a thinking budget inside the response-token ceiling for budget-based
/// thinking models. Mirrors `adjustMaxTokensForThinking`. Returns the effective
/// `max_tokens` for the request and the `budget_tokens` for the `thinking`
/// blob. When `base_max_tokens` is `None` the model cap is used and the budget
/// fits inside it; otherwise the budget is ADDED on top (capped at the model
/// max). If there's no room for the budget, shrink it (leaving MIN_ANSWER_TOKENS
/// for the answer).
pub fn adjust_max_tokens_for_thinking(
    base_max_tokens: Option<u64>,
    model: &Model,
    reasoning_level: ThinkingLevel,
    custom_budgets: Option<&crate::types::ThinkingBudgets>,
) -> (u64, u64) {
    let model_max_tokens = model.max_tokens;
    let thinking_budget = thinking_budget_for_level(model, reasoning_level, custom_budgets);
    let max_tokens = match base_max_tokens {
        None => model_max_tokens,
        Some(base) => std::cmp::min(base + thinking_budget, model_max_tokens),
    };
    let final_budget = if max_tokens <= thinking_budget {
        std::cmp::max(0, max_tokens.saturating_sub(MIN_ANSWER_TOKENS))
    } else {
        thinking_budget
    };
    (max_tokens, final_budget)
}

/// Map a pi thinking level to an Anthropic adaptive-thinking effort string.
/// Mirrors `mapThinkingLevelToEffort`. The per-model `thinking_level_map` wins
/// when it carries a non-null string for the level; otherwise minimal/low→low,
/// medium→medium, high/xhigh/max→high.
pub fn map_thinking_level_to_effort(model: &Model, level: ThinkingLevel) -> &'static str {
    if let Some(map) = &model.thinking_level_map {
        if let Some(Some(mapped)) = map.get(&level) {
            // The mapped value may be "xhigh" or "max" (only valid for models
            // that accept them). Return a matching static if recognized.
            return match mapped.as_str() {
                "xhigh" => "xhigh",
                "max" => "max",
                "low" => "low",
                "medium" => "medium",
                "high" => "high",
                _ => "high",
            };
        }
    }
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        _ => "high",
    }
}

// ----------------------------------------------------------------------------
// compute_context_tokens — minimal inline port of estimate.ts for clamp_max
// ----------------------------------------------------------------------------

/// Rough token estimate for clamp-max-tokens. Mirrors `estimateContextTokens`
/// (chars/4 per text block, image=4800 chars). Used only when the model has a
/// context window; the result feeds the `available = window - tokens - safety`
/// clamp in `simple-options.ts::clampMaxTokensToContext`.
pub fn estimate_context_tokens(messages: &[Message], system_prompt: Option<&str>) -> u64 {
    let mut chars: u64 = 0;
    if let Some(s) = system_prompt {
        chars += s.chars().count() as u64;
    }
    for msg in messages {
        match msg {
            Message::User(u) => match &u.content {
                crate::types::UserContent::Text(s) => chars += s.chars().count() as u64,
                crate::types::UserContent::Blocks(blocks) => {
                    for b in blocks {
                        match b {
                            Content::Text(t) => chars += t.text.chars().count() as u64,
                            Content::Image(_) => chars += 4800,
                            _ => {}
                        }
                    }
                }
            },
            Message::Assistant(a) => {
                for b in &a.content {
                    match b {
                        Content::Text(t) => chars += t.text.chars().count() as u64,
                        Content::Thinking(t) => chars += t.thinking.chars().count() as u64,
                        Content::Image(_) => chars += 4800,
                        _ => {}
                    }
                }
            }
            Message::ToolResult(tr) => {
                for b in &tr.content {
                    match b {
                        Content::Text(t) => chars += t.text.chars().count() as u64,
                        Content::Image(_) => chars += 4800,
                        _ => {}
                    }
                }
            }
        }
    }
    (chars + 3) / 4
}

fn clamp_max_tokens_to_context_with_tokens(model: &Model, tokens: u64, max_tokens: u64) -> u64 {
    const CONTEXT_SAFETY_TOKENS: u64 = 4096;
    const MIN_MAX_TOKENS: u64 = 1;
    let available = model
        .context_window
        .saturating_sub(tokens)
        .saturating_sub(CONTEXT_SAFETY_TOKENS);
    std::cmp::min(max_tokens, std::cmp::max(MIN_MAX_TOKENS, available))
}

/// Clamp `max_tokens` using a previously computed context-token estimate.
/// Keeping this separate lets one request reuse the estimate when thinking
/// configuration requires a second clamp.
fn clamp_max_tokens_to_context_with_estimate(
    model: &Model,
    tokens: Option<u64>,
    max_tokens: u64,
) -> u64 {
    const MIN_MAX_TOKENS: u64 = 1;
    if model.context_window == 0 {
        return std::cmp::max(MIN_MAX_TOKENS, max_tokens);
    }
    clamp_max_tokens_to_context_with_tokens(model, tokens.unwrap_or_default(), max_tokens)
}

/// Clamp `max_tokens` so the request never asks for more than the context
/// window allows (minus a safety margin). Mirrors `clampMaxTokensToContext`.
pub fn clamp_max_tokens_to_context(
    model: &Model,
    messages: &[Message],
    system_prompt: Option<&str>,
    max_tokens: u64,
) -> u64 {
    if model.context_window == 0 {
        return std::cmp::max(1, max_tokens);
    }
    let tokens = estimate_context_tokens(messages, system_prompt);
    clamp_max_tokens_to_context_with_tokens(model, tokens, max_tokens)
}

// ----------------------------------------------------------------------------
// build_params — anthropic-messages.ts:939-1074
// ----------------------------------------------------------------------------

/// Read the `AnthropicMessagesCompat` off a model (defaulting to a fully-true
/// compat when unset, matching `getAnthropicCompat`). v1 anthropic models
/// always carry a compat, but faux/unknown models default sensibly.
pub fn get_anthropic_compat(model: &Model) -> AnthropicMessagesCompat {
    model
        .compat
        .as_ref()
        .and_then(|c| match c {
            StreamingProtocolCompat::AnthropicMessages(a) => Some(a.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// True when the model's `thinking_level_map` explicitly maps `Off` to `null`
/// (meaning "off is unsupported"). Mirrors the TS `model.thinkingLevelMap?.off !== null`
/// guard in `buildParams` — when off IS mapped to null we do NOT emit an
/// explicit `thinking: { type: "disabled" }` block.
fn off_mapped_to_null(model: &Model) -> bool {
    matches!(
        model
            .thinking_level_map
            .as_ref()
            .and_then(|m| m.get(&ThinkingLevel::Off)),
        Some(None)
    )
}

/// The on-wire Anthropic request shape plus the computed cache-control block +
/// beta headers + effective max_tokens, ready for the provider to POST.
pub struct BuiltParams {
    pub request: AnthropicRequest,
    pub cache_control: CacheControlOption,
    /// The `anthropic-beta` header value (comma-joined), when any betas apply.
    pub beta_header: Option<String>,
    /// Pre-encoded body for the borrowed canonical text fast path. The normal
    /// owned request path leaves this unset.
    pub(crate) preencoded_body: Option<Vec<u8>>,
    pub(crate) preencoded_message_count: usize,
    pub diagnostics: BuildParamsDiagnostics,
}

/// Build the full Anthropic request body + beta headers from a pi context.
/// Mirrors `buildParams` + the beta-header assembly in `createClient`.
///
/// OAuth requests prepend the Claude Code identity; API-key requests preserve
/// the caller's system prompt directly.
pub fn build_params(
    model: &Model,
    ctx: &crate::types::Context,
    is_oauth: bool,
    opts: &SimpleStreamOptions,
) -> BuiltParams {
    build_params_internal(model, ctx, is_oauth, opts, false)
}

/// Build params while allowing the provider to use the borrowed canonical text
/// serializer. This is kept separate from the public owned projection used by
/// tests and callers that inspect `BuiltParams.request`.
pub(crate) fn build_params_prepared(
    model: &Model,
    ctx: &crate::types::Context,
    is_oauth: bool,
    opts: &SimpleStreamOptions,
    allow_preencoded_canonical_text: bool,
) -> BuiltParams {
    build_params_internal(model, ctx, is_oauth, opts, allow_preencoded_canonical_text)
}

fn build_params_internal(
    model: &Model,
    ctx: &crate::types::Context,
    is_oauth: bool,
    opts: &SimpleStreamOptions,
    allow_preencoded_canonical_text: bool,
) -> BuiltParams {
    let diagnostics_enabled = build_params_diagnostics_enabled();
    let total_started = diagnostics_enabled.then(Instant::now);
    let compat = get_anthropic_compat(model);
    let cache_control_opt = get_cache_control(model, opts.cache_retention);
    let cache_ref = cache_control_opt.cache_control.as_ref();

    let phase_started = diagnostics_enabled.then(Instant::now);
    let transformed = if compat.tool_references() {
        request_messages(&ctx.messages, model)
    } else {
        std::borrow::Cow::Borrowed(ctx.messages.as_slice())
    };
    let normalize_ns = elapsed_ns(phase_started);

    let phase_started = diagnostics_enabled.then(Instant::now);
    let placement = split_deferred_tools(
        &transformed,
        &ctx.tools,
        compat.tool_references(),
        &|name: &str| name.to_string(),
    );
    let deferred_tools_ns = elapsed_ns(phase_started);
    let mut immediate = placement.immediate;
    let mut deferred = placement.deferred;
    let mut deferred_names = placement.deferred_names;
    if immediate.is_empty() && !deferred.is_empty() {
        immediate = std::mem::take(&mut deferred);
        deferred_names.clear();
    }

    let phase_started = diagnostics_enabled.then(Instant::now);
    let direct_text_message_count = (allow_preencoded_canonical_text
        && !compat.tool_references()
        && !build_params_diagnostics_detail_enabled()
        // serde_json rejects non-finite f64 values. The owned request path
        // reports that encoding error; do not turn it into a fast-path panic.
        && opts.temperature.map_or(true, f64::is_finite))
    .then(|| canonical_text_message_count(&ctx.messages, model))
    .flatten();
    let direct_text = direct_text_message_count.is_some();
    let messages = if direct_text {
        Vec::new()
    } else if compat.tool_references() {
        convert_messages(
            &transformed,
            cache_ref,
            compat.empty_signature(),
            &deferred_names,
            &|name: &str| name.to_string(),
        )
    } else {
        let (wire, canonical) = convert_messages_checked(
            &ctx.messages,
            cache_ref,
            compat.empty_signature(),
            &|name: &str| name.to_string(),
            model,
        );
        if canonical {
            wire
        } else {
            let transformed = transform_messages(&ctx.messages, model);
            convert_messages(
                &transformed,
                cache_ref,
                compat.empty_signature(),
                &BTreeSet::new(),
                &|name: &str| name.to_string(),
            )
        }
    };
    let convert_messages_ns = elapsed_ns(phase_started);

    // max_tokens: opts.max_tokens wins; else model.max_tokens. Thinking may
    // inflate this (handled below for budget-based models).
    let base_max_tokens = opts.max_tokens.unwrap_or(model.max_tokens);
    let context_tokens_started = diagnostics_enabled.then(Instant::now);
    let context_tokens = (model.context_window != 0)
        .then(|| estimate_context_tokens(&ctx.messages, ctx.system_prompt.as_deref()));
    let context_tokens_ns = elapsed_ns(context_tokens_started);
    let mut max_tokens =
        clamp_max_tokens_to_context_with_estimate(model, context_tokens, base_max_tokens);

    // Thinking config.
    let thinking_enabled = opts.reasoning.is_some() && opts.reasoning != Some(ThinkingLevel::Off);
    let mut thinking: Option<AnthropicThinking> = None;
    let mut output_config: Option<Value> = None;

    if model.reasoning {
        if thinking_enabled {
            let display = "summarized".to_string();
            if compat.adaptive_thinking() {
                thinking = Some(AnthropicThinking::Adaptive { display });
                if let Some(level) = opts.reasoning {
                    let effort = map_thinking_level_to_effort(model, level);
                    if effort == "xhigh" {
                        // The Anthropic SDK types lag "xhigh"; emit it as a raw object.
                        output_config = Some(json!({ "effort": "xhigh" }));
                    } else {
                        output_config = Some(json!({ "effort": effort }));
                    }
                }
            } else {
                // Budget-based thinking.
                let reasoning_level = opts.reasoning.unwrap_or(ThinkingLevel::High);
                let (adjusted_max, budget) = adjust_max_tokens_for_thinking(
                    opts.max_tokens,
                    model,
                    reasoning_level,
                    opts.thinking_budgets.as_ref(),
                );
                // Re-clamp the adjusted max to the context window.
                max_tokens =
                    clamp_max_tokens_to_context_with_estimate(model, context_tokens, adjusted_max);
                let budget = std::cmp::min(budget, max_tokens.saturating_sub(MIN_ANSWER_TOKENS));
                thinking = Some(AnthropicThinking::Enabled {
                    budget_tokens: budget,
                    display,
                });
            }
        } else if opts.reasoning == Some(ThinkingLevel::Off) && !off_mapped_to_null(model) {
            // off is supported (not mapped to null) → explicit disabled.
            thinking = Some(AnthropicThinking::Disabled);
        }
    }

    // Subscription OAuth requires a separate CLI identity system block.
    let system: Option<Vec<AnthropicSystemBlock>> = if is_oauth {
        let mut blocks = vec![AnthropicSystemBlock {
            kind: "text".into(),
            text: "You are Claude Code, Anthropic's official CLI for Claude.".into(),
            cache_control: cache_ref.cloned(),
        }];
        if let Some(sp) = &ctx.system_prompt {
            blocks.push(AnthropicSystemBlock {
                kind: "text".into(),
                text: sanitize_surrogates(sp),
                cache_control: cache_ref.cloned(),
            });
        }
        Some(blocks)
    } else if let Some(sp) = &ctx.system_prompt {
        Some(vec![AnthropicSystemBlock {
            kind: "text".into(),
            text: sanitize_surrogates(sp),
            cache_control: cache_ref.cloned(),
        }])
    } else {
        None
    };

    // Temperature: only when not thinking and the model supports it.
    let temperature = if !thinking_enabled && compat.temperature() && opts.temperature.is_some() {
        opts.temperature
    } else {
        None
    };

    // Tools.
    let phase_started = diagnostics_enabled.then(Instant::now);
    let tools: Option<Vec<AnthropicTool>> = if !immediate.is_empty() || !deferred.is_empty() {
        let mut all = convert_tools(
            &immediate,
            compat.eager_tool_input_streaming(),
            compat.strict_tools(),
            compat.cache_control_on_tools(),
            cache_ref,
            false,
        );
        if !deferred.is_empty() {
            all.extend(convert_tools(
                &deferred,
                compat.eager_tool_input_streaming(),
                compat.strict_tools(),
                compat.cache_control_on_tools(),
                None,
                true,
            ));
        }
        Some(all)
    } else {
        None
    };
    let convert_tools_ns = elapsed_ns(phase_started);

    // Metadata user_id (abuse-tracking).
    let metadata = opts
        .metadata
        .as_ref()
        .and_then(|m| m.get("user_id"))
        .map(|v| v.as_str().to_string())
        .map(|uid| AnthropicMetadata { user_id: uid });
    // Beta headers.
    let use_fine_grained_tool_streaming_beta =
        !ctx.tools.is_empty() && !compat.eager_tool_input_streaming();
    let needs_interleaved_beta = thinking_enabled && !compat.adaptive_thinking();
    let beta_header =
        build_beta_header(use_fine_grained_tool_streaming_beta, needs_interleaved_beta);

    let request = AnthropicRequest {
        model: model.id.clone(),
        messages,
        max_tokens,
        stream: true,
        system,
        temperature,
        tools,
        thinking,
        output_config,
        metadata,
    };
    let preencoded_started = diagnostics_enabled.then(Instant::now);
    let preencoded_body = direct_text.then(|| {
        let mut body = Vec::with_capacity(canonical_text_body_capacity(&ctx.messages));
        serde_json::to_writer(
            &mut body,
            &CanonicalTextRequest {
                request: &request,
                messages: &ctx.messages,
                cache_control: cache_ref,
            },
        )
        .expect("canonical text request serialization cannot fail");
        body
    });
    let preencoded_json_ns = elapsed_ns(preencoded_started);

    BuiltParams {
        request,
        cache_control: cache_control_opt,
        beta_header,
        preencoded_body,
        preencoded_message_count: direct_text_message_count.unwrap_or_default(),
        diagnostics: BuildParamsDiagnostics {
            normalize_ns,
            deferred_tools_ns,
            convert_messages_ns,
            convert_tools_ns,
            context_tokens_ns,
            preencoded_json_ns,
            total_ns: elapsed_ns(total_started),
        },
    }
}

fn build_beta_header(fine_grained: bool, interleaved: bool) -> Option<String> {
    let mut betas: Vec<&str> = Vec::new();
    if fine_grained {
        betas.push(FINE_GRAINED_TOOL_STREAMING_BETA);
    }
    if interleaved {
        betas.push(INTERLEAVED_THINKING_BETA);
    }
    if betas.is_empty() {
        None
    } else {
        Some(betas.join(","))
    }
}

// ----------------------------------------------------------------------------
// makeStrictJsonSchema — constrained-sampling.ts port (subset)
// ----------------------------------------------------------------------------

/// Merge two JSON values: `over`'s keys win, but missing keys fall back to
/// `under`. Mirrors the TS spread `{...parameters, ...legacyInputSchema}`.
fn merge_values(over: Value, under: Value) -> Value {
    match (over, under) {
        (Value::Object(mut a), Value::Object(b)) => {
            for (k, v) in b {
                a.entry(k).or_insert(v);
            }
            Value::Object(a)
        }
        (a, _) => a,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::StreamingProtocolCompat;
    use crate::types::{
        Api, AssistantMessage, InputModality, ModelCost, Schema, StopReason, ToolResultMessage,
        UserMessage,
    };
    use serde_json::json;

    fn model_with_compat(compat: AnthropicMessagesCompat) -> Model {
        let mut m = Model::new(
            "claude-haiku-4-5",
            "Claude Haiku 4.5",
            Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        m.input = vec![InputModality::Text, InputModality::Image];
        m.context_window = 200_000;
        m.max_tokens = 8_192;
        m.cost = ModelCost::default();
        m.compat = Some(StreamingProtocolCompat::AnthropicMessages(compat));
        m
    }

    #[test]
    fn canonical_tool_history_is_borrowed_without_changing_the_normalized_messages() {
        let model = model_with_compat(Default::default());
        let mut assistant =
            AssistantMessage::empty(model.api.clone(), &model.provider, &model.id, 0);
        assistant.content = vec![
            Content::thinking("reasoning"),
            Content::tool_call("call", "echo", json!({"text":"hello"})),
        ];
        let result = ToolResultMessage {
            role: crate::types::ToolResultRole,
            tool_call_id: "call".into(),
            tool_name: "echo".into(),
            content: vec![Content::text("hello")],
            details: None,
            usage: None,
            added_tool_names: vec![],
            is_error: false,
            timestamp: 0,
        };
        let messages = vec![
            UserMessage::new("prompt", 0).into(),
            Message::Assistant(Box::new(assistant)),
            Message::ToolResult(Box::new(result)),
        ];
        let borrowed = request_messages(&messages, &model);
        assert!(matches!(borrowed, std::borrow::Cow::Borrowed(_)));
        assert_eq!(borrowed.as_ref(), transform_messages(&messages, &model));
        for end in 0..=messages.len() {
            assert_eq!(
                request_messages(&messages[..end], &model).as_ref(),
                transform_messages(&messages[..end], &model)
            );
        }
    }

    #[test]
    fn checked_canonical_conversion_matches_the_legacy_wire_projection() {
        let model = model_with_compat(Default::default());
        let mut assistant =
            AssistantMessage::empty(model.api.clone(), &model.provider, &model.id, 0);
        assistant.content = vec![
            Content::thinking("reasoning"),
            Content::tool_call("call", "echo", json!({"text":"hello"})),
        ];
        let result = ToolResultMessage {
            role: crate::types::ToolResultRole,
            tool_call_id: "call".into(),
            tool_name: "echo".into(),
            content: vec![Content::text("hello")],
            details: None,
            usage: None,
            added_tool_names: vec![],
            is_error: false,
            timestamp: 0,
        };
        let messages = vec![
            UserMessage::new("prompt", 0).into(),
            Message::Assistant(Box::new(assistant)),
            Message::ToolResult(Box::new(result)),
        ];
        let expected_messages = request_messages(&messages, &model);
        let expected = convert_messages(
            expected_messages.as_ref(),
            None,
            false,
            &BTreeSet::new(),
            &|name: &str| name.to_string(),
        );
        let (actual, canonical) = convert_messages_checked(
            &messages,
            None,
            false,
            &|name: &str| name.to_string(),
            &model,
        );
        assert!(canonical);
        assert_eq!(
            serde_json::to_vec(&actual).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
    }

    #[test]
    fn checked_conversion_accepts_consecutive_tool_results() {
        let model = model_with_compat(Default::default());
        let mut assistant =
            AssistantMessage::empty(model.api.clone(), &model.provider, &model.id, 0);
        assistant.content = vec![
            Content::tool_call("one", "echo", json!({})),
            Content::tool_call("two", "echo", json!({})),
        ];
        let result = |id: &str| {
            Message::ToolResult(Box::new(ToolResultMessage {
                role: crate::types::ToolResultRole,
                tool_call_id: id.into(),
                tool_name: "echo".into(),
                content: vec![Content::text("ok")],
                details: None,
                usage: None,
                added_tool_names: vec![],
                is_error: false,
                timestamp: 0,
            }))
        };
        let messages = vec![
            UserMessage::new("prompt", 0).into(),
            Message::Assistant(Box::new(assistant)),
            result("one"),
            result("two"),
        ];
        let (actual, canonical) = convert_messages_checked(
            &messages,
            None,
            false,
            &|name: &str| name.to_string(),
            &model,
        );
        assert!(canonical);
        let expected_messages = request_messages(&messages, &model);
        let expected = convert_messages(
            expected_messages.as_ref(),
            None,
            false,
            &BTreeSet::new(),
            &|name: &str| name.to_string(),
        );
        assert_eq!(
            serde_json::to_vec(&actual).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
    }
    #[test]
    fn borrowed_request_history_matches_normalizer_on_migrations_images_and_incomplete_tools() {
        for same_model in [false, true] {
            for stop in [StopReason::Stop, StopReason::Error, StopReason::Aborted] {
                for thinking in ["", "reasoning"] {
                    for result_id in [None, Some("call|needs-normalizing"), Some("different")] {
                        for supports_images in [false, true] {
                            let mut model = model_with_compat(Default::default());
                            if !supports_images {
                                model.input = vec![InputModality::Text];
                            }
                            let mut assistant = AssistantMessage::empty(
                                model.api.clone(),
                                &model.provider,
                                &model.id,
                                0,
                            );
                            if !same_model {
                                assistant.model = "other".into();
                            }
                            assistant.stop_reason = stop;
                            assistant.content = vec![
                                Content::thinking(thinking),
                                Content::tool_call("call|needs-normalizing", "echo", json!({})),
                            ];
                            let image = Content::Image(ImageContent {
                                kind: Default::default(),
                                data: "AA==".into(),
                                mime_type: "image/png".into(),
                            });
                            let mut messages = vec![
                                Message::User(UserMessage {
                                    role: crate::types::UserRole,
                                    content: crate::types::UserContent::Blocks(vec![image.clone()]),
                                    timestamp: 0,
                                }),
                                Message::Assistant(Box::new(assistant)),
                            ];
                            if let Some(id) = result_id {
                                messages.push(Message::ToolResult(Box::new(ToolResultMessage {
                                    role: crate::types::ToolResultRole,
                                    tool_call_id: id.into(),
                                    tool_name: "echo".into(),
                                    content: vec![image],
                                    details: None,
                                    usage: None,
                                    added_tool_names: vec![],
                                    is_error: false,
                                    timestamp: 0,
                                })));
                            }
                            messages.push(UserMessage::new("next", 0).into());
                            for end in 0..=messages.len() {
                                let prefix = &messages[..end];
                                let normalized = request_messages(prefix, &model);
                                assert_eq!(
                                    normalized.as_ref(),
                                    transform_messages(prefix, &model),
                                    "same={same_model}, stop={stop:?}, thinking={thinking:?}, result={result_id:?}, images={supports_images}, end={end}"
                                );
                                let (checked, canonical) = convert_messages_checked(
                                    prefix,
                                    None,
                                    false,
                                    &|name: &str| name.to_string(),
                                    &model,
                                );
                                let expected_input = if canonical {
                                    prefix.to_vec()
                                } else {
                                    transform_messages(prefix, &model)
                                };
                                let expected = convert_messages(
                                    &expected_input,
                                    None,
                                    false,
                                    &BTreeSet::new(),
                                    &|name: &str| name.to_string(),
                                );
                                let actual = if canonical {
                                    checked
                                } else {
                                    convert_messages(
                                        &expected_input,
                                        None,
                                        false,
                                        &BTreeSet::new(),
                                        &|name: &str| name.to_string(),
                                    )
                                };
                                assert_eq!(
                                    canonical,
                                    matches!(normalized, std::borrow::Cow::Borrowed(_)),
                                    "canonicality mismatch: same={same_model}, stop={stop:?}, thinking={thinking:?}, result={result_id:?}, images={supports_images}, end={end}"
                                );
                                assert_eq!(
                                    serde_json::to_vec(&actual).unwrap(),
                                    serde_json::to_vec(&expected).unwrap(),
                                    "checked wire mismatch: same={same_model}, stop={stop:?}, thinking={thinking:?}, result={result_id:?}, images={supports_images}, end={end}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn normalize_tool_call_id_table() {
        assert_eq!(normalize_tool_call_id("abc"), "abc");
        assert_eq!(normalize_tool_call_id("call|123|abc"), "call_123_abc");
        assert_eq!(
            normalize_tool_call_id("msg_0123456789abcdef"),
            "msg_0123456789abcdef"
        );
        // Truncation to 64 chars.
        let long: String = "a".repeat(100);
        assert_eq!(normalize_tool_call_id(&long).len(), 64);
    }

    #[test]
    fn tool_input_schema_keeps_the_definitions_a_ref_points_at() {
        // Mirrors the `edit` tool: `properties` carry `$ref`s into `#/definitions`,
        // so the definitions have to be sent too or the schema is not
        // self-contained and strict validators reject the request (DeepSeek's
        // Anthropic endpoint: "Pointer '/definitions/…' does not exist").
        let tool = Tool {
            name: "edit".into(),
            description: String::new(),
            parameters: Schema::new(json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "title": "EditInput",
                "type": "object",
                "properties": {
                    "edits": {"type": "array", "items": {"$ref": "#/definitions/ReplaceEditJson"}}
                },
                "required": ["edits"],
                "definitions": {
                    "ReplaceEditJson": {"type": "object", "properties": {"oldText": {"type": "string"}}}
                }
            })),
            constrained_sampling: None,
        };
        let schema = tool_input_schema(&tool, None);
        assert_eq!(
            schema["definitions"]["ReplaceEditJson"]["type"], "object",
            "definitions must accompany the $ref: {schema}"
        );
    }

    #[test]
    fn split_deferred_when_disabled() {
        let tools = vec![sample_tool("a"), sample_tool("b")];
        let placement = split_deferred_tools(&[], &tools, false, &|n| n.to_string());
        assert_eq!(placement.immediate.len(), 2);
        assert!(placement.deferred.is_empty());
    }

    #[test]
    fn split_deferred_single_tool_preserves_normalizer_call() {
        let calls = std::cell::Cell::new(0);
        let tools = vec![sample_tool("a")];
        let placement = split_deferred_tools(&[], &tools, false, &|name| {
            calls.set(calls.get() + 1);
            name.to_string()
        });
        assert_eq!(calls.get(), 1);
        assert_eq!(placement.immediate.len(), 1);
        assert!(placement.deferred.is_empty());
    }

    #[test]
    fn split_deferred_classifies_loaded_tools() {
        // A toolResult with added_tool_names=["b"] but no preceding assistant
        // tool call invoking "b" → "b" is deferred.
        let tr = ToolResultMessage {
            role: crate::types::ToolResultRole,
            tool_call_id: "t1".into(),
            tool_name: "a".into(),
            content: vec![Content::text("ok")],
            details: None,
            usage: None,
            added_tool_names: vec!["b".into()],
            is_error: false,
            timestamp: 0,
        };
        let messages = vec![Message::ToolResult(Box::new(tr))];
        let tools = vec![sample_tool("a"), sample_tool("b")];
        let placement = split_deferred_tools(&messages, &tools, true, &|n| n.to_string());
        assert_eq!(placement.immediate.len(), 1);
        assert_eq!(placement.immediate[0].name, "a");
        assert_eq!(placement.deferred.len(), 1);
        assert_eq!(placement.deferred[0].name, "b");
        assert!(placement.deferred_names.contains("b"));
    }

    #[test]
    fn convert_tools_attaches_cache_control_on_last_immediate_only() {
        let tools = vec![sample_tool("a"), sample_tool("b")];
        let cc = CacheControlEphemeral {
            kind: crate::providers::anthropic::cache_control::EphemeralType,
            ttl: None,
        };
        let wire = convert_tools(&tools, true, false, true, Some(&cc), false);
        assert_eq!(wire.len(), 2);
        assert!(wire[0].cache_control.is_none());
        assert!(wire[1].cache_control.is_some());
        // eager_input_streaming set when supported.
        assert_eq!(wire[0].eager_input_streaming, Some(true));
    }

    #[test]
    fn convert_tools_defer_loading_flag() {
        let tools = vec![sample_tool("a")];
        let wire = convert_tools(&tools, true, false, true, None, true);
        assert_eq!(wire[0].defer_loading, Some(true));
        // No cache_control on deferred.
        assert!(wire[0].cache_control.is_none());
    }

    #[test]
    fn build_params_temperature_omitted_under_thinking() {
        let mut compat = AnthropicMessagesCompat::default();
        compat.supports_temperature = Some(true);
        let mut model = model_with_compat(compat);
        model.reasoning = true;
        let ctx = crate::types::Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage::new("hi", 0))],
            tools: vec![],
        };
        let mut opts = SimpleStreamOptions::default();
        opts.reasoning = Some(ThinkingLevel::Medium);
        opts.temperature = Some(0.7);
        let built = build_params(&model, &ctx, false, &opts);
        assert!(built.request.temperature.is_none());
        assert!(built.request.thinking.is_some());
    }

    #[test]
    fn build_params_adaptive_thinking_emits_effort() {
        let mut compat = AnthropicMessagesCompat::default();
        compat.force_adaptive_thinking = Some(true);
        let mut model = model_with_compat(compat);
        model.reasoning = true;
        let ctx = crate::types::Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage::new("hi", 0))],
            tools: vec![],
        };
        let mut opts = SimpleStreamOptions::default();
        opts.reasoning = Some(ThinkingLevel::High);
        let built = build_params(&model, &ctx, false, &opts);
        assert!(matches!(
            built.request.thinking,
            Some(AnthropicThinking::Adaptive { .. })
        ));
        assert_eq!(
            built.request.output_config,
            Some(json!({ "effort": "high" }))
        );
    }

    #[test]
    fn build_params_budget_thinking_emits_budget_tokens() {
        let compat = AnthropicMessagesCompat::default(); // force_adaptive=false
        let mut model = model_with_compat(compat);
        model.reasoning = true;
        let ctx = crate::types::Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage::new("hi", 0))],
            tools: vec![],
        };
        let mut opts = SimpleStreamOptions::default();
        opts.reasoning = Some(ThinkingLevel::High);
        let built = build_params(&model, &ctx, false, &opts);
        match built.request.thinking {
            Some(AnthropicThinking::Enabled { budget_tokens, .. }) => {
                assert!(budget_tokens > 0);
            }
            other => panic!("expected Enabled, got {other:?}"),
        }
    }

    #[test]
    fn tool_image_conversion_and_sibling_displacement_preserve_content() {
        let image = Content::Image(ImageContent {
            kind: Default::default(),
            data: "aW1hZ2U=".into(),
            mime_type: "image/png".into(),
        });
        let wire_image = json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": "aW1hZ2U="}
        });
        for (content, expected) in [
            (vec![image.clone()], json!([wire_image.clone()])),
            (
                vec![
                    Content::text("before"),
                    image.clone(),
                    Content::text("after"),
                ],
                json!([
                    {"type": "text", "text": "before"}, wire_image.clone(),
                    {"type": "text", "text": "after"}
                ]),
            ),
            (
                vec![image.clone(), image],
                json!([wire_image.clone(), wire_image]),
            ),
        ] {
            let converted = convert_content_blocks(&content);
            assert_eq!(serde_json::to_value(&converted).unwrap(), expected);
            assert_eq!(
                serde_json::to_value(sibling_content_from(converted)).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn text_only_and_nonvision_tool_content_remain_plain_strings() {
        let content = vec![Content::text("before"), Content::text("after")];
        assert_eq!(
            serde_json::to_value(convert_content_blocks(&content)).unwrap(),
            json!("before\nafter")
        );
        let images = vec![Content::Image(ImageContent {
            kind: Default::default(),
            data: "aW1hZ2U=".into(),
            mime_type: "image/png".into(),
        })];
        assert_eq!(
            serde_json::to_value(convert_content_blocks(&downgrade_tool_result_images(
                &images
            )))
            .unwrap(),
            json!("(tool image omitted: model does not support images)")
        );
    }

    #[test]
    fn adjust_max_tokens_fits_budget_in_model_cap() {
        // No caller cap, model cap 8192, high budget 16384 → max=8192, budget shrinks.
        let model = model_with_compat(Default::default());
        let (max, budget) = adjust_max_tokens_for_thinking(None, &model, ThinkingLevel::High, None);
        assert_eq!(max, 8192);
        assert!(budget < 16384);
    }

    #[test]
    fn xhigh_and_max_budgets_apply_only_to_models_that_offer_them() {
        let mut model = model_with_compat(Default::default());
        model.max_tokens = 128_000;
        let budget = |model: &Model, level, custom| {
            adjust_max_tokens_for_thinking(Some(4096), model, level, custom).1
        };
        assert_eq!(
            budget(&model, ThinkingLevel::Max, None),
            16384,
            "unoffered levels clamp to high"
        );
        model.thinking_level_map = Some(
            [(ThinkingLevel::Xhigh, Some("xhigh".to_string()))]
                .into_iter()
                .collect(),
        );
        assert_eq!(budget(&model, ThinkingLevel::Xhigh, None), 32768);
        assert_eq!(
            budget(&model, ThinkingLevel::Max, None),
            32768,
            "max falls back to xhigh"
        );
        let custom = crate::types::ThinkingBudgets {
            xhigh: Some(40_000),
            ..Default::default()
        };
        assert_eq!(budget(&model, ThinkingLevel::Xhigh, Some(&custom)), 40_000);
    }

    #[test]
    fn transform_messages_inserts_synthetic_result_for_orphan_tool_call() {
        let mut compat = AnthropicMessagesCompat::default();
        compat.supports_tool_references = Some(false);
        let model = model_with_compat(compat);
        let assistant =
            AssistantMessage::empty(Api::AnthropicMessages, "anthropic", "claude-haiku-4-5", 0);
        let mut a = assistant;
        a.stop_reason = StopReason::ToolUse;
        a.content = vec![Content::tool_call("t1", "search", json!({}))];
        // Same model so the tool call id isn't remapped.
        let messages = vec![Message::Assistant(Box::new(a))];
        let out = transform_messages(&messages, &model);
        // Assistant + synthetic toolResult.
        assert_eq!(out.len(), 2);
        match &out[1] {
            Message::ToolResult(tr) => {
                assert!(tr.is_error);
                assert_eq!(tr.tool_call_id, "t1");
                assert_eq!(tr.content.len(), 1);
            }
            _ => panic!("expected toolResult"),
        }
    }

    #[test]
    fn borrowed_text_request_is_byte_identical_to_owned_projection() {
        let mut compat = AnthropicMessagesCompat::default();
        compat.supports_tool_references = Some(false);
        let model = model_with_compat(compat);
        let mut assistant =
            AssistantMessage::empty(model.api.clone(), &model.provider, &model.id, 0);
        assistant.content = vec![Content::text("answer\nwith unicode 🐈")];
        let empty_assistant =
            AssistantMessage::empty(model.api.clone(), &model.provider, &model.id, 1);
        let mut ctx = crate::types::Context {
            system_prompt: Some("system\n🧪".into()),
            messages: vec![
                UserMessage::new("  prompt  ", 0).into(),
                UserMessage::new("   ", 0).into(),
                Message::Assistant(Box::new(assistant)),
                Message::Assistant(Box::new(empty_assistant)),
            ],
            tools: vec![sample_tool("search")],
        };
        let mut opts = SimpleStreamOptions::default();
        opts.max_tokens = Some(1234);
        opts.temperature = Some(0.25);
        opts.metadata = Some(std::collections::BTreeMap::from([(
            "user_id".into(),
            "test-user".into(),
        )]));

        let legacy = build_params(&model, &ctx, false, &opts);
        let prepared = build_params_prepared(&model, &ctx, false, &opts, true);
        let legacy_bytes = serde_json::to_vec(&legacy.request).unwrap();
        let prepared_bytes = prepared.preencoded_body.as_ref().unwrap();

        assert_eq!(prepared.preencoded_message_count, 2);
        assert_eq!(prepared_bytes, &legacy_bytes);
        assert_eq!(
            legacy.request.messages.len(),
            prepared.preencoded_message_count
        );
        ctx.messages.push(UserMessage::new("tail", 0).into());
        assert_ne!(
            prepared_bytes,
            &serde_json::to_vec(&build_params(&model, &ctx, false, &opts).request).unwrap()
        );
    }

    #[test]
    fn borrowed_text_request_preserves_cache_control_on_final_user() {
        let mut compat = AnthropicMessagesCompat::default();
        compat.supports_tool_references = Some(false);
        let model = model_with_compat(compat);
        let ctx = crate::types::Context::new(vec![UserMessage::new("final", 0).into()]);
        let opts = SimpleStreamOptions::default();

        let legacy = build_params(&model, &ctx, false, &opts);
        let prepared = build_params_prepared(&model, &ctx, false, &opts, true);
        assert_eq!(
            prepared.preencoded_body.as_ref().unwrap(),
            &serde_json::to_vec(&legacy.request).unwrap()
        );
    }

    #[test]
    fn borrowed_text_request_is_byte_identical_with_thinking() {
        let mut compat = AnthropicMessagesCompat::default();
        compat.supports_tool_references = Some(false);
        let mut model = model_with_compat(compat);
        model.reasoning = true;
        let ctx = crate::types::Context::new(vec![UserMessage::new("prompt", 0).into()]);
        let mut opts = SimpleStreamOptions::default();
        opts.reasoning = Some(ThinkingLevel::High);
        opts.max_tokens = Some(4096);

        let legacy = build_params(&model, &ctx, false, &opts);
        let prepared = build_params_prepared(&model, &ctx, false, &opts, true);
        assert_eq!(
            prepared.preencoded_body.as_ref().unwrap(),
            &serde_json::to_vec(&legacy.request).unwrap()
        );
    }

    #[test]
    fn borrowed_text_serializer_is_disabled_for_tool_references() {
        let model = model_with_compat(AnthropicMessagesCompat::default());
        let ctx = crate::types::Context::new(vec![UserMessage::new("prompt", 0).into()]);
        let prepared =
            build_params_prepared(&model, &ctx, false, &SimpleStreamOptions::default(), true);
        assert!(prepared.preencoded_body.is_none());
        assert_eq!(prepared.request.messages.len(), 1);
    }

    #[test]
    fn borrowed_text_serializer_falls_back_for_non_finite_temperature() {
        let mut compat = AnthropicMessagesCompat::default();
        compat.supports_tool_references = Some(false);
        let model = model_with_compat(compat);
        let ctx = crate::types::Context::new(vec![UserMessage::new("prompt", 0).into()]);
        let mut opts = SimpleStreamOptions::default();
        opts.temperature = Some(f64::NAN);
        let prepared = build_params_prepared(&model, &ctx, false, &opts, true);
        assert!(prepared.preencoded_body.is_none());
        assert_eq!(prepared.request.messages.len(), 1);
    }

    #[test]
    fn borrowed_text_serializer_falls_back_for_non_text_history() {
        let mut compat = AnthropicMessagesCompat::default();
        compat.supports_tool_references = Some(false);
        let model = model_with_compat(compat);
        let mut assistant =
            AssistantMessage::empty(model.api.clone(), &model.provider, &model.id, 0);
        assistant.content = vec![Content::thinking("reasoning")];
        let ctx = crate::types::Context::new(vec![Message::Assistant(Box::new(assistant))]);
        let legacy = build_params(&model, &ctx, false, &SimpleStreamOptions::default());
        let prepared =
            build_params_prepared(&model, &ctx, false, &SimpleStreamOptions::default(), true);

        assert!(prepared.preencoded_body.is_none());
        assert_eq!(
            serde_json::to_vec(&prepared.request).unwrap(),
            serde_json::to_vec(&legacy.request).unwrap()
        );
    }

    #[test]
    fn all_deferred_tools_fall_back_to_immediate_without_references() {
        let model = model_with_compat(AnthropicMessagesCompat::default());
        let ctx = crate::types::Context {
            system_prompt: None,
            messages: vec![Message::ToolResult(Box::new(ToolResultMessage {
                role: crate::types::ToolResultRole,
                tool_call_id: "call".into(),
                tool_name: "loader".into(),
                content: vec![Content::text("loaded")],
                details: None,
                usage: None,
                added_tool_names: vec!["loader".into()],
                is_error: false,
                timestamp: 0,
            }))],
            tools: vec![sample_tool("loader")],
        };
        let built = build_params(&model, &ctx, false, &SimpleStreamOptions::default());
        assert_eq!(built.request.tools.as_ref().unwrap()[0].name, "loader");
        assert_eq!(built.request.tools.as_ref().unwrap()[0].defer_loading, None);
        assert!(matches!(
            &built.request.messages[0].content,
            AnthropicMessageContent::Blocks(blocks)
                if matches!(&blocks[0], AnthropicContentBlock::ToolResult {
                    content: Some(ToolResultContent::Text(text)),
                    ..
                } if text == "loaded")
        ));
    }

    fn sample_tool(name: &str) -> Tool {
        Tool {
            name: name.into(),
            description: "sample".into(),
            parameters: Schema::new(json!({
                "type": "object",
                "properties": { "x": { "type": "string" } },
                "required": ["x"],
            })),
            constrained_sampling: None,
        }
    }
}
