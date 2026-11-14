//! Mirrors `packages/ai/src/types.ts` — the core LLM type contract every other
//! crate consumes.
//!
//! These types are the boundary between the provider-agnostic agent loop and the
//! provider wire formats. Everything the loop talks in (`Content`, `Message`,
//! `AssistantMessage`, `AssistantMessageEvent`) is defined here; provider code
//! only produces/consumes these.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

/// A newtype wrapping a JSON Schema value. Tools carry their parameters schema here
/// (produced by `schemars::JsonSchema` derive, stored as a `serde_json::Value` so it
/// can be sent over the wire to providers verbatim). Mirrors the TypeBox `TSchema`
/// parameter shape on the TS `Tool`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct Schema(pub serde_json::Value);

impl Schema {
    pub fn new(value: serde_json::Value) -> Self {
        Self(value)
    }

    pub fn empty_object() -> Self {
        Self(serde_json::Value::Object(serde_json::Map::new()))
    }

    pub fn as_value(&self) -> &serde_json::Value {
        &self.0
    }

    pub fn into_value(self) -> serde_json::Value {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_null()
            || (self.0.is_object() && self.0.as_object().map(|m| m.is_empty()).unwrap_or(true))
    }
}

impl From<serde_json::Value> for Schema {
    fn from(v: serde_json::Value) -> Self {
        Self(v)
    }
}

// ----------------------------------------------------------------------------
// Content blocks — mirror TS `TextContent | ImageContent | ThinkingContent | ToolCall`
// ----------------------------------------------------------------------------

/// A text content block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextContent {
    // `kind` renames to "type" so direct serialization carries the block
    // discriminator. `default`: `Content` is `#[serde(tag="type")]`, and the
    // tagged path consumes the `type` field as the tag before the payload is
    // deserialized — without a default, re-reading a persisted `Content`
    // failed with "missing field `type`" (sessions could be written but never
    // restored). The unit-marker default is the correct value per variant.
    #[serde(default, rename = "type")]
    pub kind: TextContentType,
    pub text: String,
    /// Provider message-metadata signature (OpenAI Responses legacy id string or
    /// `TextSignatureV1` JSON). Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_signature: Option<String>,
}

/// Marker so the `type` field serializes as the literal `"text"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TextContentType;
impl Serialize for TextContentType {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("text")
    }
}
impl<'de> Deserialize<'de> for TextContentType {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s != "text" {
            return Err(serde::de::Error::custom(format!(
                "expected \"text\", got {s:?}"
            )));
        }
        Ok(Self)
    }
}

/// Thinking / reasoning content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
    #[serde(default, rename = "type")]
    pub kind: ThinkingContentType,
    pub thinking: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
    /// When true, safety filters redacted the thinking; the opaque payload is in
    /// `thinking_signature`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub redacted: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThinkingContentType;
impl Serialize for ThinkingContentType {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("thinking")
    }
}
impl<'de> Deserialize<'de> for ThinkingContentType {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s != "thinking" {
            return Err(serde::de::Error::custom(format!(
                "expected \"thinking\", got {s:?}"
            )));
        }
        Ok(Self)
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// An image content block (base64-encoded).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    // `Content` is internally tagged and consumes `type` before deserializing
    // this struct. Default the marker just like text/thinking content so
    // persisted image blocks round-trip through JSONL.
    #[serde(default, rename = "type")]
    pub kind: ImageContentType,
    /// Base64. Shared, because messages are cloned between the session, the
    /// agent loop and the providers, and one image outweighs all their text.
    pub data: Arc<str>,
    pub mime_type: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImageContentType;
impl Serialize for ImageContentType {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("image")
    }
}
impl<'de> Deserialize<'de> for ImageContentType {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s != "image" {
            return Err(serde::de::Error::custom(format!(
                "expected \"image\", got {s:?}"
            )));
        }
        Ok(Self)
    }
}

/// A tool call request from the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    #[serde(default, rename = "type")]
    pub kind: ToolCallType,
    pub id: String,
    pub name: String,
    /// LLM-supplied arguments. `Value::Object` in the happy path; we permit any
    /// `Value` so partial-parse recovery has somewhere to park malformed input.
    pub arguments: serde_json::Value,
    /// Google-specific opaque signature for reusing thought context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
    /// OpenAI Responses namespace for dynamic/namespaced tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Recorded result of a call that already ran upstream (for example, a
    /// remote agent's tool execution streamed back to the host). When present,
    /// the host must never execute the call: the recording becomes its tool
    /// result, or an error result if the recording is invalid. Its presence is
    /// the signal, so an explicit `null` counts as a (malformed) recording.
    /// Absent on ordinary calls. The agent loop's `recorded_tool_result` decodes
    /// the accepted shape.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_present_json"
    )]
    pub recorded: Option<serde_json::Value>,
}

/// Deserialize a field that is present as-is, so an explicit JSON `null` becomes
/// `Some(Value::Null)` rather than being mistaken for an absent field.
pub(crate) fn deserialize_present_json<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<serde_json::Value>, D::Error> {
    serde_json::Value::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolCallType;
impl Serialize for ToolCallType {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("toolCall")
    }
}
impl<'de> Deserialize<'de> for ToolCallType {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s != "toolCall" {
            return Err(serde::de::Error::custom(format!(
                "expected \"toolCall\", got {s:?}"
            )));
        }
        Ok(Self)
    }
}

/// `TextContent | ImageContent | ThinkingContent | ToolCall` — assistant-side
/// content blocks. User messages use only text/image; assistant messages may carry
/// all four. Tagged via the `type` field so it round-trips the TS union exactly.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Content {
    Text(TextContent),
    Thinking(ThinkingContent),
    Image(ImageContent),
    ToolCall(ToolCall),
}

// Each concrete block already serializes its own `type` marker. Deriving an
// internally tagged serializer here would emit that field a second time, which
// raw JSON readers reject (a Value-based round-trip silently hides the bug).
impl Serialize for Content {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Text(block) => block.serialize(serializer),
            Self::Thinking(block) => block.serialize(serializer),
            Self::Image(block) => block.serialize(serializer),
            Self::ToolCall(block) => block.serialize(serializer),
        }
    }
}

impl Content {
    pub fn text<S: Into<String>>(s: S) -> Self {
        Content::Text(TextContent {
            kind: TextContentType,
            text: s.into(),
            text_signature: None,
        })
    }

    pub fn thinking<S: Into<String>>(s: S) -> Self {
        Content::Thinking(ThinkingContent {
            kind: ThinkingContentType,
            thinking: s.into(),
            thinking_signature: None,
            redacted: false,
        })
    }

    pub fn tool_call<I: Into<String>, N: Into<String>>(
        id: I,
        name: N,
        arguments: serde_json::Value,
    ) -> Self {
        Content::ToolCall(ToolCall {
            kind: ToolCallType,
            id: id.into(),
            name: name.into(),
            arguments,
            thought_signature: None,
            namespace: None,
            recorded: None,
        })
    }

    /// Plain-text extraction (mirrors `contentText`): joins all `Text` blocks with
    /// `\n`, dropping thinking/tool-call/image blocks.
    pub fn text_only(content: &[Content], sep: &str) -> String {
        content
            .iter()
            .filter_map(|c| match c {
                Content::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(sep)
    }
}

// ----------------------------------------------------------------------------
// Usage + costs
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostRates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCostTier {
    #[serde(flatten)]
    pub rates: ModelCostRates,
    pub input_tokens_above: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    #[serde(flatten)]
    pub rates: ModelCostRates,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<ModelCostTier>,
}

impl Default for ModelCost {
    fn default() -> Self {
        Self {
            rates: ModelCostRates::default(),
            tiers: Vec::new(),
        }
    }
}

/// Per-request token usage + cost. Mirrors TS `Usage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    /// Subset of `cache_write` written with 1h retention (Anthropic only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h: Option<i64>,
    /// Reasoning tokens, when reported. Subset of `output`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<i64>,
    pub total_tokens: i64,
    pub cost: UsageCost,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

impl Usage {
    pub fn zero() -> Self {
        Self {
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: 0,
            cost: UsageCost::default(),
        }
    }

    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        if other.cache_write_1h.is_some() {
            self.cache_write_1h =
                Some(self.cache_write_1h.unwrap_or(0) + other.cache_write_1h.unwrap());
        }
        if other.reasoning.is_some() {
            self.reasoning = Some(self.reasoning.unwrap_or(0) + other.reasoning.unwrap());
        }
        self.total_tokens += other.total_tokens;
        self.cost.input += other.cost.input;
        self.cost.output += other.cost.output;
        self.cost.cache_read += other.cost.cache_read;
        self.cost.cache_write += other.cost.cache_write;
        self.cost.total += other.cost.total;
    }

    /// `usage.totalTokens || (input + output + cacheRead + cacheWrite)` — mirrors
    /// `calculateContextTokens`.
    pub fn context_tokens(&self) -> i64 {
        let sum = self.total_tokens;
        if sum > 0 {
            sum
        } else {
            self.input + self.output + self.cache_read + self.cache_write
        }
    }
}

// ----------------------------------------------------------------------------
// Stop reason + provider/api identifiers
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StopReason {
    Pending,
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
    Deferred,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::Pending => "pending",
            StopReason::Stop => "stop",
            StopReason::Length => "length",
            StopReason::ToolUse => "toolUse",
            StopReason::Error => "error",
            StopReason::Aborted => "aborted",
            StopReason::Deferred => "deferred",
        }
    }
}

/// Pi known APIs. The string fallback covers custom APIs; we keep a typed enum for
/// the common ones and expose `Api::Other(String)` for the rest.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Api {
    OpenaiCompletions,
    MistralConversations,
    OpenaiResponses,
    AzureOpenaiResponses,
    OpenaiCodexResponses,
    AnthropicMessages,
    BedrockConverseStream,
    GoogleGenerativeAi,
    GoogleVertex,
    PiMessages,
    Faux,
    Other(String),
}

impl Api {
    pub fn as_str(&self) -> &str {
        match self {
            Api::OpenaiCompletions => "openai-completions",
            Api::MistralConversations => "mistral-conversations",
            Api::OpenaiResponses => "openai-responses",
            Api::AzureOpenaiResponses => "azure-openai-responses",
            Api::OpenaiCodexResponses => "openai-codex-responses",
            Api::AnthropicMessages => "anthropic-messages",
            Api::BedrockConverseStream => "bedrock-converse-stream",
            Api::GoogleGenerativeAi => "google-generative-ai",
            Api::GoogleVertex => "google-vertex",
            Api::PiMessages => "pi-messages",
            Api::Faux => "faux",
            Api::Other(s) => s.as_str(),
        }
    }
}

impl Serialize for Api {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}
impl<'de> Deserialize<'de> for Api {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(match s.as_str() {
            "openai-completions" => Api::OpenaiCompletions,
            "mistral-conversations" => Api::MistralConversations,
            "openai-responses" => Api::OpenaiResponses,
            "azure-openai-responses" => Api::AzureOpenaiResponses,
            "openai-codex-responses" => Api::OpenaiCodexResponses,
            "anthropic-messages" => Api::AnthropicMessages,
            "bedrock-converse-stream" => Api::BedrockConverseStream,
            "google-generative-ai" => Api::GoogleGenerativeAi,
            "google-vertex" => Api::GoogleVertex,
            "pi-messages" => Api::PiMessages,
            "faux" => Api::Faux,
            other => Api::Other(other.to_string()),
        })
    }
}

/// Provider identifier (string; keeps the TS `KnownProvider | string` open enum).
pub type ProviderId = String;

/// Input modalities a model accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputModality {
    Text,
    Image,
}

/// Thinking levels, mirroring TS `ThinkingLevel | "off"`.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

/// Maps pi thinking levels to provider-specific values; `None` marks a level as
/// unsupported. Mirrors `ThinkingLevelMap = Partial<Record<...>>`.
pub type ThinkingLevelMap = BTreeMap<ThinkingLevel, Option<String>>;

/// Custom token budgets per thinking level for token-budgeted reasoning models.
/// Mirrors TS `ThinkingBudgets`; all fields optional (defaults live in
/// `simple-options.ts::adjustMaxTokensForThinking`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingBudgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimal: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high: Option<u64>,
    /// Used only for models whose `thinkingLevelMap` offers `xhigh`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xhigh: Option<u64>,
    /// Used only for models whose `thinkingLevelMap` offers `max`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u64>,
}

// ----------------------------------------------------------------------------
// Deferred responses
// ----------------------------------------------------------------------------

/// A durable handle to a deferred provider response (long-poll APIs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredHandle {
    pub provider: String,
    pub model_id: String,
    pub api: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_after_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

// ----------------------------------------------------------------------------
// Messages — mirror TS `Message = UserMessage | AssistantMessage | ToolResultMessage`
// ----------------------------------------------------------------------------

/// User content may be a plain string or a list of text/image blocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Blocks(Vec<Content>),
}

impl UserContent {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            UserContent::Text(s) => Some(s),
            UserContent::Blocks(_) => None,
        }
    }
}

impl From<String> for UserContent {
    fn from(s: String) -> Self {
        UserContent::Text(s)
    }
}

impl From<&str> for UserContent {
    fn from(s: &str) -> Self {
        UserContent::Text(s.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    #[serde(rename = "role")]
    pub role: UserRole,
    pub content: UserContent,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UserRole;
impl Serialize for UserRole {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("user")
    }
}
impl<'de> Deserialize<'de> for UserRole {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s != "user" {
            return Err(serde::de::Error::custom(format!(
                "expected \"user\", got {s:?}"
            )));
        }
        Ok(Self)
    }
}

impl UserMessage {
    pub fn new(content: impl Into<UserContent>, timestamp: i64) -> Self {
        Self {
            role: UserRole,
            content: content.into(),
            timestamp,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    #[serde(rename = "role")]
    pub role: ToolResultRole,
    pub tool_call_id: String,
    pub tool_name: String,
    /// Result content (text/image blocks).
    pub content: Vec<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added_tool_names: Vec<String>,
    pub is_error: bool,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolResultRole;
impl Serialize for ToolResultRole {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("toolResult")
    }
}
impl<'de> Deserialize<'de> for ToolResultRole {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s != "toolResult" {
            return Err(serde::de::Error::custom(format!(
                "expected \"toolResult\", got {s:?}"
            )));
        }
        Ok(Self)
    }
}

/// Concrete assistant message. Mirrors TS `AssistantMessage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    #[serde(rename = "role")]
    pub role: AssistantRole,
    pub content: Vec<Content>,
    pub api: Api,
    pub provider: ProviderId,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredHandle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_turn: Option<bool>,
    pub timestamp: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssistantRole;
impl Serialize for AssistantRole {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("assistant")
    }
}
impl<'de> Deserialize<'de> for AssistantRole {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s != "assistant" {
            return Err(serde::de::Error::custom(format!(
                "expected \"assistant\", got {s:?}"
            )));
        }
        Ok(Self)
    }
}

impl AssistantMessage {
    /// Construct a new assistant message with zeroed usage and `Pending` stop reason,
    /// ready for the stream mapper to mutate per-event.
    pub fn empty(
        api: Api,
        provider: impl Into<String>,
        model: impl Into<String>,
        timestamp: i64,
    ) -> Self {
        Self {
            role: AssistantRole,
            content: Vec::new(),
            api,
            provider: provider.into(),
            model: model.into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: StopReason::Pending,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp,
        }
    }

    /// Convenience: an error/aborted terminal message (used by providers + faux).
    pub fn terminal(
        api: Api,
        provider: impl Into<String>,
        model: impl Into<String>,
        stop_reason: StopReason,
        error_message: impl Into<String>,
        timestamp: i64,
    ) -> Self {
        Self {
            role: AssistantRole,
            content: Vec::new(),
            api,
            provider: provider.into(),
            model: model.into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason,
            deferred: None,
            error_message: Some(error_message.into()),
            raw_stop_reason: None,
            end_turn: None,
            timestamp,
        }
    }
}

/// `Message = UserMessage | AssistantMessage | ToolResultMessage`. The base
/// provider-facing message type.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    User(UserMessage),
    Assistant(Box<AssistantMessage>),
    ToolResult(Box<ToolResultMessage>),
}

// The concrete message structs intentionally retain their `role` field so
// they serialize exactly like Pi's TypeScript messages. An internally tagged
// enum would consume that field before deserializing the struct variant,
// causing `missing field role` for otherwise valid messages. Decode the tag
// explicitly and pass the complete object through to the concrete type.
impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let role = value
            .get("role")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| serde::de::Error::missing_field("role"))?;
        match role {
            "user" => serde_json::from_value(value)
                .map(Message::User)
                .map_err(serde::de::Error::custom),
            "assistant" => serde_json::from_value(value)
                .map(|message| Message::Assistant(Box::new(message)))
                .map_err(serde::de::Error::custom),
            "toolResult" => serde_json::from_value(value)
                .map(|message| Message::ToolResult(Box::new(message)))
                .map_err(serde::de::Error::custom),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["user", "assistant", "toolResult"],
            )),
        }
    }
}

impl From<UserMessage> for Message {
    fn from(m: UserMessage) -> Self {
        Message::User(m)
    }
}
impl From<AssistantMessage> for Message {
    fn from(m: AssistantMessage) -> Self {
        Message::Assistant(Box::new(m))
    }
}
impl From<ToolResultMessage> for Message {
    fn from(m: ToolResultMessage) -> Self {
        Message::ToolResult(Box::new(m))
    }
}

// ----------------------------------------------------------------------------
// Tool + Context
// ----------------------------------------------------------------------------

/// OpenAI grammar variants — mirror TS `GrammarFormat` (used by constrained sampling).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum GrammarFormat {
    #[serde(rename = "openai_lark")]
    OpenaiLark,
    #[serde(rename = "openai_regex")]
    OpenaiRegex,
}

pub type GrammarVariants = BTreeMap<GrammarFormat, String>;

/// Provider-side constrained sampling config (json_schema strict, or grammar).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConstrainedSamplingConfig {
    JsonSchema {
        #[serde(rename = "strict")]
        strict: ConstrainedStrictness,
    },
    Grammar {
        variants: GrammarVariants,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConstrainedStrictness {
    Prefer,
    Require,
}

/// A tool definition. Mirrors TS `Tool`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Schema,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constrained_sampling: Option<ConstrainedSamplingConfig>,
}

/// The context handed to a provider stream call. Mirrors TS `Context`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
}

impl Context {
    pub fn new(messages: Vec<Message>) -> Self {
        Self {
            system_prompt: None,
            messages,
            tools: Vec::new(),
        }
    }
}

// ----------------------------------------------------------------------------
// AssistantMessageEvent — the streaming protocol union
// ----------------------------------------------------------------------------

/// `AssistantMessageEvent` tagged union, mirrors TS exactly.
///
/// Providers emit events in this order: `Start` is emitted once before any per-block
/// events, then a `{Start,Delta*,End}` triple per content block (text / thinking /
/// toolcall), then a terminal `Done` (carrying the final successful message) or
/// `Error` (carrying a terminal message with `StopReason::Error`/`Aborted`).
///
/// Every non-terminal event references the in-progress `AssistantMessage` via
/// `partial` so subscribers can render a live view. We wrap the partial in `Arc`
/// so the event is cheap to clone across broadcast subscribers.
#[derive(Debug, Clone, PartialEq)]
pub enum AssistantMessageEvent {
    Start {
        partial: std::sync::Arc<AssistantMessage>,
    },
    TextStart {
        content_index: usize,
        partial: std::sync::Arc<AssistantMessage>,
    },
    TextDelta {
        content_index: usize,
        delta: String,
        partial: std::sync::Arc<AssistantMessage>,
    },
    TextEnd {
        content_index: usize,
        content: String,
        partial: std::sync::Arc<AssistantMessage>,
    },
    ThinkingStart {
        content_index: usize,
        partial: std::sync::Arc<AssistantMessage>,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
        partial: std::sync::Arc<AssistantMessage>,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
        partial: std::sync::Arc<AssistantMessage>,
    },
    ToolCallStart {
        content_index: usize,
        partial: std::sync::Arc<AssistantMessage>,
    },
    ToolCallDelta {
        content_index: usize,
        delta: String,
        partial: std::sync::Arc<AssistantMessage>,
    },
    ToolCallEnd {
        content_index: usize,
        tool_call: ToolCall,
        partial: std::sync::Arc<AssistantMessage>,
    },
    Done {
        reason: DoneReason,
        message: AssistantMessage,
    },
    Error {
        reason: ErrorReason,
        error: AssistantMessage,
    },
}

/// The terminal reason on a `Done` event — the success subset of `StopReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DoneReason {
    Stop,
    Length,
    ToolUse,
    Deferred,
}

impl DoneReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DoneReason::Stop => "stop",
            DoneReason::Length => "length",
            DoneReason::ToolUse => "toolUse",
            DoneReason::Deferred => "deferred",
        }
    }
}

impl From<DoneReason> for StopReason {
    fn from(r: DoneReason) -> Self {
        match r {
            DoneReason::Stop => StopReason::Stop,
            DoneReason::Length => StopReason::Length,
            DoneReason::ToolUse => StopReason::ToolUse,
            DoneReason::Deferred => StopReason::Deferred,
        }
    }
}

/// The terminal reason on an `Error` event — the failure subset of `StopReason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorReason {
    Aborted,
    Error,
}

impl ErrorReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorReason::Aborted => "aborted",
            ErrorReason::Error => "error",
        }
    }
}

impl From<ErrorReason> for StopReason {
    fn from(r: ErrorReason) -> Self {
        match r {
            ErrorReason::Aborted => StopReason::Aborted,
            ErrorReason::Error => StopReason::Error,
        }
    }
}

impl AssistantMessageEvent {
    /// Returns the `type` tag, mirroring the TS discriminator.
    pub fn type_tag(&self) -> &'static str {
        match self {
            AssistantMessageEvent::Start { .. } => "start",
            AssistantMessageEvent::TextStart { .. } => "text_start",
            AssistantMessageEvent::TextDelta { .. } => "text_delta",
            AssistantMessageEvent::TextEnd { .. } => "text_end",
            AssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
            AssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
            AssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
            AssistantMessageEvent::ToolCallStart { .. } => "toolcall_start",
            AssistantMessageEvent::ToolCallDelta { .. } => "toolcall_delta",
            AssistantMessageEvent::ToolCallEnd { .. } => "toolcall_end",
            AssistantMessageEvent::Done { .. } => "done",
            AssistantMessageEvent::Error { .. } => "error",
        }
    }

    /// True for the two terminal variants.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
        )
    }

    /// Snapshot of the partial message for live rendering. Terminal events do not
    /// carry a `partial`; returns the terminal message itself for `Done`/`Error`.
    pub fn partial(&self) -> &AssistantMessage {
        match self {
            AssistantMessageEvent::Start { partial }
            | AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolCallStart { partial, .. }
            | AssistantMessageEvent::ToolCallDelta { partial, .. }
            | AssistantMessageEvent::ToolCallEnd { partial, .. } => partial,
            AssistantMessageEvent::Done { message, .. } => message,
            AssistantMessageEvent::Error { error, .. } => error,
        }
    }
}

impl AssistantMessageEvent {
    /// Accounted in-memory payload, without serialization or allocation. Shared
    /// snapshots are charged for each queued event. This is a queue budget, not RSS:
    /// capacities, allocator overhead and consumer-held/final messages are excluded.
    pub fn accounted_payload_bytes(&self) -> usize {
        let extra = match self {
            Self::TextDelta { delta, .. }
            | Self::ThinkingDelta { delta, .. }
            | Self::ToolCallDelta { delta, .. } => delta.len(),
            Self::TextEnd { content, .. } | Self::ThinkingEnd { content, .. } => content.len(),
            Self::ToolCallEnd { tool_call, .. } => tool_payload_bytes(tool_call),
            _ => 0,
        };
        std::mem::size_of::<Self>() + message_payload_bytes(self.partial()) + extra
    }
}

fn json_payload_bytes(value: &serde_json::Value) -> usize {
    use serde_json::Value;
    std::mem::size_of::<Value>()
        + match value {
            Value::String(s) => s.len(),
            Value::Array(values) => values.iter().map(json_payload_bytes).sum(),
            Value::Object(values) => values
                .iter()
                .map(|(k, v)| k.len() + json_payload_bytes(v))
                .sum(),
            _ => 0,
        }
}

fn tool_payload_bytes(tool: &ToolCall) -> usize {
    tool.id.len()
        + tool.name.len()
        + json_payload_bytes(&tool.arguments)
        + tool.thought_signature.as_ref().map_or(0, String::len)
        + tool.namespace.as_ref().map_or(0, String::len)
}

fn message_payload_bytes(message: &AssistantMessage) -> usize {
    let mut bytes = std::mem::size_of::<AssistantMessage>()
        + message.api.as_str().len()
        + message.provider.len()
        + message.model.len();
    for value in [
        &message.response_model,
        &message.response_id,
        &message.error_message,
        &message.raw_stop_reason,
    ] {
        bytes += value.as_ref().map_or(0, String::len);
    }
    if let Some(handle) = &message.deferred {
        bytes += std::mem::size_of::<DeferredHandle>()
            + handle.provider.len()
            + handle.model_id.len()
            + handle.api.len()
            + handle.id.len()
            + handle.data.as_ref().map_or(0, json_payload_bytes);
    }
    for content in &message.content {
        bytes += std::mem::size_of::<Content>()
            + match content {
                Content::Text(text) => {
                    text.text.len() + text.text_signature.as_ref().map_or(0, String::len)
                }
                Content::Thinking(thinking) => {
                    thinking.thinking.len()
                        + thinking.thinking_signature.as_ref().map_or(0, String::len)
                }
                Content::Image(image) => image.data.len() + image.mime_type.len(),
                Content::ToolCall(tool) => tool_payload_bytes(tool),
            };
    }
    bytes
}

// serde for AssistantMessageEvent — tagged on "type" matching the wire protocol.
impl Serialize for AssistantMessageEvent {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let tag = self.type_tag();
        match self {
            AssistantMessageEvent::Start { partial } => {
                let mut m = s.serialize_map(Some(2))?;
                m.serialize_entry("type", tag)?;
                m.serialize_entry("partial", &**partial)?;
                m.end()
            }
            AssistantMessageEvent::TextStart {
                content_index,
                partial,
            }
            | AssistantMessageEvent::ThinkingStart {
                content_index,
                partial,
            }
            | AssistantMessageEvent::ToolCallStart {
                content_index,
                partial,
            } => {
                let mut m = s.serialize_map(Some(3))?;
                m.serialize_entry("type", tag)?;
                m.serialize_entry("contentIndex", content_index)?;
                m.serialize_entry("partial", &**partial)?;
                m.end()
            }
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                partial,
            }
            | AssistantMessageEvent::ThinkingDelta {
                content_index,
                delta,
                partial,
            } => {
                let mut m = s.serialize_map(Some(4))?;
                m.serialize_entry("type", tag)?;
                m.serialize_entry("contentIndex", content_index)?;
                m.serialize_entry("delta", delta)?;
                m.serialize_entry("partial", &**partial)?;
                m.end()
            }
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
                partial,
            } => {
                let mut m = s.serialize_map(Some(4))?;
                m.serialize_entry("type", tag)?;
                m.serialize_entry("contentIndex", content_index)?;
                m.serialize_entry("delta", delta)?;
                m.serialize_entry("partial", &**partial)?;
                m.end()
            }
            AssistantMessageEvent::TextEnd {
                content_index,
                content,
                partial,
            }
            | AssistantMessageEvent::ThinkingEnd {
                content_index,
                content,
                partial,
            } => {
                let mut m = s.serialize_map(Some(4))?;
                m.serialize_entry("type", tag)?;
                m.serialize_entry("contentIndex", content_index)?;
                m.serialize_entry("content", content)?;
                m.serialize_entry("partial", &**partial)?;
                m.end()
            }
            AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call,
                partial,
            } => {
                let mut m = s.serialize_map(Some(4))?;
                m.serialize_entry("type", tag)?;
                m.serialize_entry("contentIndex", content_index)?;
                m.serialize_entry("toolCall", tool_call)?;
                m.serialize_entry("partial", &**partial)?;
                m.end()
            }
            AssistantMessageEvent::Done { reason, message } => {
                let mut m = s.serialize_map(Some(3))?;
                m.serialize_entry("type", tag)?;
                m.serialize_entry("reason", reason.as_str())?;
                m.serialize_entry("message", message)?;
                m.end()
            }
            AssistantMessageEvent::Error { reason, error } => {
                let mut m = s.serialize_map(Some(3))?;
                m.serialize_entry("type", tag)?;
                m.serialize_entry("reason", reason.as_str())?;
                m.serialize_entry("error", error)?;
                m.end()
            }
        }
    }
}
