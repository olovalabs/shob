//! Mirrors `packages/ai/src/models.ts` — `Model<TApi>` + the compatibility structs.
//!
//! The compat structs (`AnthropicMessagesCompat`, etc.) are kept here rather than in
//! `types.rs` because they are model-layer configuration, not the core message
//! contract. Providers read them off `Model` to decide request shaping.

use crate::types::{Api, InputModality, ModelCost, ProviderId, ThinkingLevelMap};
use serde::{Deserialize, Serialize};

/// Compatibility flags for Anthropic Messages-compatible APIs.
/// Mirrors TS `AnthropicMessagesCompat`. `Default` matches the documented defaults
/// at each field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct AnthropicMessagesCompat {
    /// Default: true.
    pub supports_eager_tool_input_streaming: Option<bool>,
    /// Default: true.
    pub supports_long_cache_retention: Option<bool>,
    /// Default: false.
    pub send_session_affinity_headers: Option<bool>,
    /// Default: true.
    pub supports_cache_control_on_tools: Option<bool>,
    /// Default: true.
    pub supports_temperature: Option<bool>,
    /// Default: false.
    pub force_adaptive_thinking: Option<bool>,
    /// Default: false.
    pub allow_empty_signature: Option<bool>,
    /// Default: false.
    pub supports_strict_tools: Option<bool>,
    /// Default: true for first-party Anthropic (except Haiku / Claude ≤ 4.4); false otherwise.
    pub supports_tool_references: Option<bool>,
}

impl AnthropicMessagesCompat {
    /// Resolve a flag to its effective value given the documented default.
    pub fn eager_tool_input_streaming(&self) -> bool {
        self.supports_eager_tool_input_streaming.unwrap_or(true)
    }
    pub fn long_cache_retention(&self) -> bool {
        self.supports_long_cache_retention.unwrap_or(true)
    }
    pub fn cache_control_on_tools(&self) -> bool {
        self.supports_cache_control_on_tools.unwrap_or(true)
    }
    pub fn temperature(&self) -> bool {
        self.supports_temperature.unwrap_or(true)
    }
    pub fn adaptive_thinking(&self) -> bool {
        self.force_adaptive_thinking.unwrap_or(false)
    }
    pub fn empty_signature(&self) -> bool {
        self.allow_empty_signature.unwrap_or(false)
    }
    pub fn strict_tools(&self) -> bool {
        self.supports_strict_tools.unwrap_or(false)
    }
    pub fn tool_references(&self) -> bool {
        // Mirrors defaultSupportsToolReferences: true for first-party Anthropic
        // except Haiku and Claude ≤ 4.4. The model catalog overrides this for
        // non-Anthropic providers (false).
        self.supports_tool_references.unwrap_or(true)
    }
}

/// Whether the next request carries the model's own prior reasoning back to it.
///
/// This decides what the system prompt may claim about the working-state
/// channel: when reasoning *is* replayed, telling the model "reasoning is not
/// carried into your next turn" is simply false, and the instruction to restate
/// the plan every turn becomes busy-work (the re-plan symptom in
/// `docs/debugging/llm-repetition-forensics.md`).
///
/// - `openai-completions`: with `compat.requiresThinkingAsText` (text parts) or
///   `compat.requiresReasoningContent` (a separate reasoning_content field).
/// - `openai-responses`: visible thinking is replayed as input content.
/// - `anthropic-messages`: thinking blocks are echoed back with their signature.
/// - everything else: reasoning is dropped, so the model cannot see it again.
pub fn prior_reasoning_is_replayed(model: &Model) -> bool {
    match model.api {
        Api::OpenaiCompletions => matches!(
            model.compat.as_ref(),
            Some(StreamingProtocolCompat::OpenaiCompletions(compat))
                if compat.requires_thinking_as_text == Some(true)
                    || compat.requires_reasoning_content == Some(true)
        ),
        Api::OpenaiResponses | Api::AzureOpenaiResponses | Api::OpenaiCodexResponses => true,
        Api::AnthropicMessages => true,
        _ => false,
    }
}

/// Compatibility flags for OpenAI Completions-compat APIs. Only a subset is
/// modeled for v1 (we have no OpenAI provider yet); the rest live here as
/// `Option`s for forward-compat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct OpenaiCompletionsCompat {
    pub supports_store: Option<bool>,
    pub supports_developer_role: Option<bool>,
    pub supports_reasoning_effort: Option<bool>,
    pub supports_usage_in_streaming: Option<bool>,
    pub supports_finish_reason: Option<bool>,
    pub max_tokens_field: Option<MaxTokensField>,
    pub requires_tool_result_name: Option<bool>,
    pub requires_assistant_after_tool_result: Option<bool>,
    pub requires_thinking_as_text: Option<bool>,
    /// Replay prior assistant thinking in the wire reasoning_content field.
    /// Opt-in per model; ordinary OpenAI endpoints keep their existing shape.
    #[serde(alias = "requiresReasoningContentOnAssistantMessages")]
    pub requires_reasoning_content: Option<bool>,
    /// Request format for thinking. Unset keeps the OpenAI request shape.
    pub thinking_format: Option<OpenaiThinkingFormat>,
    /// Whether the endpoint counts hidden reasoning tokens against the same
    /// completion budget `max_tokens` describes (DeepSeek v4 reasoning models,
    /// several OpenAI-compat relays). When set, a request that enables thinking
    /// reserves the level's reasoning budget on top of `Model::max_tokens` so a
    /// long chain of thought cannot starve the answer into
    /// `finish_reason: "length"` with an empty or clipped message.
    /// Opt-in per model; unset keeps the previous single-budget behaviour.
    pub reasoning_counts_toward_max_tokens: Option<bool>,
    pub supports_strict_mode: Option<bool>,
    pub send_session_affinity_headers: Option<bool>,
    pub supports_long_cache_retention: Option<bool>,
}

/// Supported thinking request formats for OpenAI-compatible endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpenaiThinkingFormat {
    /// Send `reasoning_effort` when supported.
    Openai,
    /// Also send `thinking: { type: "enabled" | "disabled" }`.
    Deepseek,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    MaxCompletionTokens,
    MaxTokens,
}

/// Compatibility flags for OpenAI Responses-compat APIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct OpenaiResponsesCompat {
    pub supports_developer_role: Option<bool>,
    pub supports_long_cache_retention: Option<bool>,
    pub supports_strict_mode: Option<bool>,
    pub supports_reasoning_effort: Option<bool>,
    pub omit_reasoning_effort: Option<bool>,
    pub include_encrypted_reasoning: Option<bool>,
    /// Whether this endpoint accepts OpenAI custom tools with Lark/regex
    /// grammar formats. Grammar tools fall back to ordinary function tools
    /// when this is unset or false.
    #[serde(rename = "supportsOpenAIGrammarTools")]
    pub supports_openai_grammar_tools: Option<bool>,
}

/// Compatibility flags for Bedrock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct BedrockCompat {
    pub supports_strict_mode: Option<bool>,
}

/// Per-API compat blob. Matches the TS `compat?` discriminator on `Model`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamingProtocolCompat {
    AnthropicMessages(AnthropicMessagesCompat),
    OpenaiCompletions(OpenaiCompletionsCompat),
    OpenaiResponses(OpenaiResponsesCompat),
    Bedrock(BedrockCompat),
    /// Custom / unknown API compat (opaque passthrough).
    Other(serde_json::Value),
    /// No compat configured (default for faux and simple providers).
    None,
}

impl Default for StreamingProtocolCompat {
    fn default() -> Self {
        StreamingProtocolCompat::None
    }
}

impl StreamingProtocolCompat {
    pub fn as_anthropic(&self) -> Option<&AnthropicMessagesCompat> {
        match self {
            StreamingProtocolCompat::AnthropicMessages(c) => Some(c),
            _ => None,
        }
    }
}

/// A model entry. Mirrors TS `Model<TApi>`; the generic is erased to a runtime
/// `Api` field so a single `Model` works for every provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: Api,
    pub provider: ProviderId,
    pub base_url: String,
    pub reasoning: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    pub input: Vec<InputModality>,
    pub cost: ModelCost,
    /// Whether pricing was explicitly supplied, including intentionally free models.
    #[serde(default)]
    pub cost_configured: bool,
    pub context_window: u64,
    pub max_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_params: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<StreamingProtocolCompat>,
}

impl Model {
    /// Construct a minimal model with empty cost + zeroed windows.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        api: Api,
        provider: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            api,
            provider: provider.into(),
            base_url: base_url.into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![InputModality::Text],
            cost: ModelCost::default(),
            cost_configured: false,
            context_window: 0,
            max_tokens: 0,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    /// Explicit zero rates and built-in nonzero rates both count as known pricing.
    pub fn has_pricing(&self) -> bool {
        self.cost_configured || self.cost != ModelCost::default()
    }

    /// Returns the supported thinking levels for this model. Mirrors
    /// `getSupportedThinkingLevels`: `xhigh`/`max` are only listed when their map
    /// entry is a non-null string; other levels are supported unless mapped to `null`.
    pub fn supported_thinking_levels(&self) -> Vec<crate::types::ThinkingLevel> {
        use crate::types::ThinkingLevel::*;
        let map = self.thinking_level_map.as_ref();
        let supported = |lvl: crate::types::ThinkingLevel| -> bool {
            match map.and_then(|m| m.get(&lvl)) {
                Some(None) => false,   // explicitly null → unsupported
                Some(Some(_)) => true, // mapped → supported
                None => true,          // absent → use provider default (supported)
            }
        };
        let mut levels = vec![Off, Minimal, Low, Medium, High];
        // xhigh/max only when their map entry is a non-null string.
        if matches!(map.and_then(|m| m.get(&Xhigh)), Some(Some(_))) {
            levels.push(Xhigh);
        }
        if matches!(map.and_then(|m| m.get(&Max)), Some(Some(_))) {
            levels.push(Max);
        }
        levels.retain(|&lvl| supported(lvl));
        levels
    }

    /// Clamp a requested thinking level to a supported one. Mirrors
    /// `clampThinkingLevel`: walks the ordered list and accepts if supported,
    /// otherwise falls back to the nearest lower supported level.
    pub fn clamp_thinking_level(
        &self,
        requested: crate::types::ThinkingLevel,
    ) -> crate::types::ThinkingLevel {
        use crate::types::ThinkingLevel::*;
        let order = [Off, Minimal, Low, Medium, High, Xhigh, Max];
        let supported: std::collections::HashSet<_> =
            self.supported_thinking_levels().into_iter().collect();
        let req_idx = order
            .iter()
            .position(|l| *l == requested)
            .unwrap_or(order.len() - 1);
        // Walk from requested down to find nearest supported.
        for i in (0..=req_idx).rev() {
            if supported.contains(&order[i]) {
                return order[i];
            }
        }
        Off
    }
}
