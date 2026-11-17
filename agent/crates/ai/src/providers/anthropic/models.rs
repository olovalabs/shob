//! Hand-curated representative subset of the Anthropic model catalog.
//!
//! The TS source is auto-generated from `scripts/generate-models.ts` reading a
//! `data/anthropic.json` snapshot (sourced from models.dev). That snapshot is
//! not vendored in the read-only reference clone, so for v1 we ship a small
//! representative set whose compat flags, thinking-level maps, and cost rates
//! match what the generator would emit for the same ids — verified against the
//! generator rules in `scripts/generate-models.ts` and the cost fixtures in
//! `test/anthropic-cache-write-1h-cost.test.ts`.
//!
//! TODO: replace with a generated catalog (models.dev snapshot) once the data
//! file is available; the per-id compat math here is a faithful stopgap.

use crate::model::{AnthropicMessagesCompat, Model, StreamingProtocolCompat};
use crate::types::{
    Api, InputModality, ModelCost, ModelCostRates, ThinkingLevel, ThinkingLevelMap,
};
use std::collections::BTreeMap;

const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_PROVIDER: &str = "anthropic";

/// Convenience accessor for the catalog, mirroring `ANTHROPIC_MODELS`.
pub fn anthropic_models() -> Vec<Model> {
    vec![
        // ---- Current adaptive-thinking lineup ----
        claude_opus_4_8(),
        claude_opus_4_7(),
        claude_opus_5(),
        claude_sonnet_5(),
        claude_fable_5(),
        // ---- Budget-based thinking + prior generation ----
        claude_sonnet_4_5(),
        claude_haiku_4_5(),
    ]
}

/// Look up a model by id. Mirrors the `getModel("anthropic", id)` shape tests use.
pub fn get_model(id: &str) -> Option<Model> {
    anthropic_models().into_iter().find(|m| m.id == id)
}

// ----------------------------------------------------------------------------
// Cost-rate construction
// ----------------------------------------------------------------------------

fn cost_rates(input: f64, output: f64, cache_read: f64, cache_write: f64) -> ModelCostRates {
    ModelCostRates {
        input,
        output,
        cache_read,
        cache_write,
    }
}

fn cost(rates: ModelCostRates) -> ModelCost {
    ModelCost {
        rates,
        tiers: Vec::new(),
    }
}

// ----------------------------------------------------------------------------
// Compat + thinking-level-map construction
// ----------------------------------------------------------------------------

/// Build an `AnthropicMessagesCompat` with only the non-default flags set, so
/// the `Option`-based accessors resolve exactly the values the generator would
/// merge. Defaults not listed here resolve to their documented defaults
/// (`true` for most, `false` for `force_adaptive_thinking`/`strict_tools`).
fn compat(force_adaptive: bool, supports_temperature: bool) -> AnthropicMessagesCompat {
    let mut c = AnthropicMessagesCompat::default();
    if force_adaptive {
        c.force_adaptive_thinking = Some(true);
    }
    if !supports_temperature {
        c.supports_temperature = Some(false);
    }
    // Anthropic first-party supports strict tools (the generator sets
    // supportsStrictTools=true for first-party anthropic-messages models).
    c.supports_strict_tools = Some(true);
    c
}

fn map_xhigh_max() -> ThinkingLevelMap {
    let mut m = BTreeMap::new();
    m.insert(ThinkingLevel::Xhigh, Some("xhigh".to_string()));
    m.insert(ThinkingLevel::Max, Some("max".to_string()));
    m
}

fn map_max() -> ThinkingLevelMap {
    let mut m = BTreeMap::new();
    m.insert(ThinkingLevel::Max, Some("max".to_string()));
    m
}

fn map_fable5() -> ThinkingLevelMap {
    let mut m = BTreeMap::new();
    m.insert(ThinkingLevel::Off, None);
    m.insert(ThinkingLevel::Xhigh, Some("xhigh".to_string()));
    m.insert(ThinkingLevel::Max, Some("max".to_string()));
    m
}

fn finish(
    id: &str,
    name: &str,
    reasoning: bool,
    thinking_level_map: Option<ThinkingLevelMap>,
    compat: AnthropicMessagesCompat,
    rates: ModelCostRates,
    context_window: u64,
    max_tokens: u64,
) -> Model {
    let mut m = Model::new(
        id,
        name,
        Api::AnthropicMessages,
        ANTHROPIC_PROVIDER,
        ANTHROPIC_BASE_URL,
    );
    m.reasoning = reasoning;
    m.thinking_level_map = thinking_level_map;
    m.input = vec![InputModality::Text, InputModality::Image];
    m.cost = cost(rates);
    m.context_window = context_window;
    m.max_tokens = max_tokens;
    m.compat = Some(StreamingProtocolCompat::AnthropicMessages(compat));
    m
}

// ----------------------------------------------------------------------------
// Per-model builders
// ----------------------------------------------------------------------------

/// `claude-opus-4-8` — adaptive thinking, temperature unsupported, 2x 1h cache.
/// Rates verified against `test/anthropic-cache-write-1h-cost.test.ts`.
pub fn claude_opus_4_8() -> Model {
    finish(
        "claude-opus-4-8",
        "Claude Opus 4.8",
        true,
        Some(map_xhigh_max()),
        compat(true, false),
        // input 5, output 25, cacheRead 0.5, cacheWrite (5m) 6.25 per Mtok.
        cost_rates(5.0, 25.0, 0.5, 6.25),
        200_000,
        32_000,
    )
}

/// `claude-opus-4-7` — adaptive thinking, temperature unsupported.
pub fn claude_opus_4_7() -> Model {
    finish(
        "claude-opus-4-7",
        "Claude Opus 4.7",
        true,
        Some(map_xhigh_max()),
        compat(true, false),
        cost_rates(5.0, 25.0, 0.5, 6.25),
        200_000,
        32_000,
    )
}

/// `claude-opus-5` — adaptive thinking, temperature unsupported.
pub fn claude_opus_5() -> Model {
    finish(
        "claude-opus-5",
        "Claude Opus 5",
        true,
        Some(map_xhigh_max()),
        compat(true, false),
        cost_rates(5.0, 25.0, 0.5, 6.25),
        200_000,
        32_000,
    )
}

/// `claude-sonnet-5` — adaptive thinking, temperature supported.
pub fn claude_sonnet_5() -> Model {
    finish(
        "claude-sonnet-5",
        "Claude Sonnet 5",
        true,
        Some(map_xhigh_max()),
        compat(true, true),
        cost_rates(3.0, 15.0, 0.3, 3.75),
        200_000,
        16_000,
    )
}

/// `claude-fable-5` — adaptive thinking, off unsupported, xhigh + max.
pub fn claude_fable_5() -> Model {
    finish(
        "claude-fable-5",
        "Claude Fable 5",
        true,
        Some(map_fable5()),
        compat(true, true),
        cost_rates(3.0, 15.0, 0.3, 3.75),
        200_000,
        16_000,
    )
}

/// `claude-sonnet-4-5` — budget-based thinking, temperature supported. Not in
/// the adaptive set (forceAdaptiveThinking stays false).
pub fn claude_sonnet_4_5() -> Model {
    finish(
        "claude-sonnet-4-5",
        "Claude Sonnet 4.5",
        true,
        Some(map_max()),
        compat(false, true),
        cost_rates(3.0, 15.0, 0.3, 3.75),
        200_000,
        16_000,
    )
}

/// `claude-haiku-4-5` — budget-based thinking, temperature supported, no tool
/// references (Haiku rejects client-side tool_reference blocks).
pub fn claude_haiku_4_5() -> Model {
    let mut c = compat(false, true);
    c.supports_tool_references = Some(false);
    finish(
        "claude-haiku-4-5",
        "Claude Haiku 4.5",
        true,
        Some(map_max()),
        c,
        cost_rates(1.0, 5.0, 0.1, 1.25),
        200_000,
        8_192,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::StreamingProtocolCompat;

    #[test]
    fn catalog_contains_representative_ids() {
        let models = anthropic_models();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        for expected in [
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-fable-5",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
        ] {
            assert!(ids.contains(&expected), "missing {expected}");
        }
    }

    #[test]
    fn get_model_resolves_known_id() {
        let m = get_model("claude-opus-4-8").expect("opus 4.8 present");
        assert_eq!(m.provider, "anthropic");
        assert_eq!(m.api, Api::AnthropicMessages);
        // Cache-write 1h cost fixture relies on these exact rates.
        assert_eq!(m.cost.rates.input, 5.0);
        assert_eq!(m.cost.rates.cache_write, 6.25);
    }

    #[test]
    fn opus_4_8_is_adaptive_and_temperatureless() {
        let m = claude_opus_4_8();
        let c = m
            .compat
            .as_ref()
            .and_then(|c| match c {
                StreamingProtocolCompat::AnthropicMessages(a) => Some(a),
                _ => None,
            })
            .unwrap();
        assert!(c.adaptive_thinking());
        assert!(!c.temperature());
        // Opus 4.7+ carries xhigh + max effort mappings.
        let map = m.thinking_level_map.as_ref().unwrap();
        assert_eq!(
            map.get(&ThinkingLevel::Xhigh),
            Some(&Some("xhigh".to_string()))
        );
        assert_eq!(map.get(&ThinkingLevel::Max), Some(&Some("max".to_string())));
    }

    #[test]
    fn haiku_4_5_disables_tool_references() {
        let m = claude_haiku_4_5();
        let c = m
            .compat
            .as_ref()
            .and_then(|c| match c {
                StreamingProtocolCompat::AnthropicMessages(a) => Some(a),
                _ => None,
            })
            .unwrap();
        assert!(!c.tool_references());
        assert!(!c.adaptive_thinking());
        assert!(c.temperature());
    }

    #[test]
    fn fable_5_marks_off_unsupported() {
        let m = claude_fable_5();
        let map = m.thinking_level_map.as_ref().unwrap();
        assert_eq!(map.get(&ThinkingLevel::Off), Some(&None));
        assert!(m
            .supported_thinking_levels()
            .iter()
            .all(|&l| l != ThinkingLevel::Off));
    }

    #[test]
    fn strict_tools_set_for_first_party() {
        for m in anthropic_models() {
            let c = m
                .compat
                .as_ref()
                .and_then(|c| match c {
                    StreamingProtocolCompat::AnthropicMessages(a) => Some(a),
                    _ => None,
                })
                .unwrap();
            assert!(c.strict_tools(), "{} should support strict tools", m.id);
        }
    }
}
