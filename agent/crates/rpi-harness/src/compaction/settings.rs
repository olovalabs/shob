//! Mirrors `packages/agent/src/harness/compaction/compaction.ts:246-250`
//! (`shouldCompact`) and the `CompactionSettings`/`DEFAULT_COMPACTION_SETTINGS`
//! re-export.
//!
//! The struct + constant themselves live in [`crate::types`] (declared there so
//! `AgentHarnessOptions`, which predates this module, can carry the field
//! without a forward dependency). This module owns only the threshold check and
//! re-exports the struct for a stable `rpi_harness::compaction::settings` path.

// Re-export so `super::mod` can surface them as `compaction::CompactionSettings`.
pub use crate::types::{CompactionSettings, DEFAULT_COMPACTION_SETTINGS};

/// Return whether context usage exceeds the configured compaction threshold.
/// Mirrors TS `shouldCompact(contextTokens, contextWindow, settings)`:
/// `if (!settings.enabled) return false; return contextTokens > contextWindow -
/// settings.reserveTokens;`.
///
/// `context_tokens` and `context_window` are `i64` (token counts). The
/// subtraction is plain (NOT saturating): when `reserve_tokens > context_window`
/// the threshold goes negative, so ANY `context_tokens >= 0` trips compaction —
/// exactly the TS behavior. Realistic token counts (millions) cannot overflow
/// `i64`, so plain subtraction is safe.
pub fn should_compact(
    context_tokens: i64,
    context_window: i64,
    settings: &CompactionSettings,
) -> bool {
    if !settings.enabled {
        return false;
    }
    let threshold = context_window - settings.reserve_tokens;
    context_tokens > threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_never_compacts() {
        let s = CompactionSettings {
            enabled: false,
            reserve_tokens: 1000,
            keep_recent_tokens: 500,
        };
        assert!(!should_compact(9_999_999, 10_000, &s));
    }

    #[test]
    fn trips_above_threshold() {
        let s = CompactionSettings {
            enabled: true,
            reserve_tokens: 1000,
            keep_recent_tokens: 500,
        };
        // window 10000 - reserve 1000 = 9000 threshold.
        assert!(!should_compact(9000, 10000, &s));
        assert!(should_compact(9001, 10000, &s));
        assert!(should_compact(99999, 10000, &s));
    }

    #[test]
    fn reserve_exceeding_window_trips_on_any_usage() {
        // Mirrors TS: threshold = contextWindow - reserve = 10000 - 20000 = -10000,
        // and `contextTokens > -10000` is true for every non-negative context_tokens,
        // so compaction trips even at zero usage.
        let s = CompactionSettings {
            enabled: true,
            reserve_tokens: 20_000,
            keep_recent_tokens: 500,
        };
        assert!(should_compact(0, 10_000, &s));
        assert!(should_compact(1, 10_000, &s));
    }

    #[test]
    fn defaults_match_ts() {
        assert_eq!(DEFAULT_COMPACTION_SETTINGS.enabled, true);
        assert_eq!(DEFAULT_COMPACTION_SETTINGS.reserve_tokens, 16384);
        assert_eq!(DEFAULT_COMPACTION_SETTINGS.keep_recent_tokens, 20000);
    }
}
