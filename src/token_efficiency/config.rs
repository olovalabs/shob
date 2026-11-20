//! Per-feature switches for the token-efficiency layer.
//!
//! Every capability is independently switchable and defaults to the safe value.
//! `trust_mtime` and `anchor_edits` default off/on respectively for different
//! reasons: `trust_mtime` trades correctness for a stat, so it must be opted
//! into; `anchor_edits` is pure savings once the ledger exists, so it is on —
//! but the *prompt hint* that tells the model the syntax exists is separate
//! (see the design notes in `src/token_efficiency/README.md`).

use serde::{Deserialize, Serialize};

/// The environment switch that disables the whole layer for one run without
/// touching `settings.json`. Follows the existing `SHOB_*` convention.
pub const ENV_DISABLE: &str = "SHOB_TOKEN_EFFICIENCY";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenEfficiencySettings {
    /// Master switch. `false` makes every hook a no-op.
    pub enabled: bool,
    /// Return "unchanged since turn N" instead of re-sending a file body.
    pub read_dedupe: bool,
    /// Short-circuit an identical `grep`/`find` whose scanned set is unchanged.
    pub search_cache: bool,
    /// Accept `PUT`/`CUT` line-anchor edit operations and expand them locally.
    pub anchor_edits: bool,
    /// Collapse noisy `bash`/`grep`/`find`/`ls` output.
    pub minimize_output: bool,
    /// Route cheap roles (title generation) to the configured small model.
    pub route_cheap_tasks: bool,
    /// Record per-turn token and cache-hit rows.
    pub metrics: bool,

    /// Treat an unchanged `mtime` + byte length as proof the content is
    /// unchanged, skipping the hash. Faster, and wrong when a file is rewritten
    /// with the same length inside one millisecond. Off by default.
    pub trust_mtime: bool,
    /// How many recent turns a delivered body is assumed to still be in
    /// context. Past this distance a re-read is sent in full.
    pub dedupe_window_turns: u32,
    /// Refuse to validate a search entry whose scanned set is larger than this;
    /// validating would cost more I/O than searching again.
    pub max_validate_files: usize,
    /// Only minimize a tool result larger than this.
    pub minimize_over_bytes: usize,
    /// Hard cap on a minimized tool result.
    pub minimize_max_bytes: usize,
    /// `provider/model` selector for the cheap role. `None` = not routed.
    pub smol_model: Option<String>,
}

impl Default for TokenEfficiencySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            read_dedupe: true,
            search_cache: true,
            anchor_edits: true,
            minimize_output: true,
            route_cheap_tasks: true,
            metrics: true,
            trust_mtime: false,
            dedupe_window_turns: 8,
            max_validate_files: 2000,
            minimize_over_bytes: 4096,
            minimize_max_bytes: 24 * 1024,
            smol_model: None,
        }
    }
}

impl TokenEfficiencySettings {
    /// The effective settings for this process: the environment switch wins
    /// over the file, so a single run can be compared against the old behavior
    /// without editing settings.
    pub fn resolved(self) -> Self {
        if env_disabled() {
            return Self {
                enabled: false,
                ..self
            };
        }
        self
    }

    /// True when no feature is active, so callers can skip building hooks.
    pub fn is_inert(&self) -> bool {
        !self.enabled
            || !(self.read_dedupe
                || self.search_cache
                || self.anchor_edits
                || self.minimize_output
                || self.metrics)
    }
}

/// `SHOB_TOKEN_EFFICIENCY=0|false|off|no` disables the layer.
pub fn env_disabled() -> bool {
    match std::env::var(ENV_DISABLE) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_enable_the_savings_and_disable_the_risky_shortcut() {
        let settings = TokenEfficiencySettings::default();
        assert!(settings.enabled);
        assert!(settings.read_dedupe);
        assert!(settings.search_cache);
        assert!(settings.anchor_edits);
        assert!(settings.minimize_output);
        assert!(settings.metrics);
        assert!(!settings.trust_mtime, "mtime trust must be opt-in");
        assert_eq!(settings.dedupe_window_turns, 8);
        assert_eq!(settings.minimize_over_bytes, 4096);
        assert_eq!(settings.minimize_max_bytes, 24 * 1024);
        assert_eq!(settings.smol_model, None);
    }

    #[test]
    fn missing_fields_deserialize_to_defaults() {
        // A settings file written before this feature existed must still load.
        let settings: TokenEfficiencySettings = serde_json::from_str("{}").expect("empty object");
        assert_eq!(settings, TokenEfficiencySettings::default());

        let partial: TokenEfficiencySettings =
            serde_json::from_str(r#"{"read_dedupe": false}"#).expect("partial");
        assert!(!partial.read_dedupe);
        assert!(partial.search_cache, "unset fields keep their default");
    }

    #[test]
    fn already_absent_implies_absent() {
        let settings: TokenEfficiencySettings = serde_json::from_str("null").unwrap_or_default();
        assert_eq!(settings, TokenEfficiencySettings::default());
    }

    #[test]
    fn inert_when_disabled_or_all_features_off() {
        let mut settings = TokenEfficiencySettings::default();
        assert!(!settings.is_inert());

        settings.enabled = false;
        assert!(settings.is_inert());

        let everything_off = TokenEfficiencySettings {
            enabled: true,
            read_dedupe: false,
            search_cache: false,
            anchor_edits: false,
            minimize_output: false,
            route_cheap_tasks: false,
            metrics: false,
            ..TokenEfficiencySettings::default()
        };
        assert!(everything_off.is_inert());
    }

    #[test]
    fn metrics_alone_keeps_the_layer_active() {
        let settings = TokenEfficiencySettings {
            read_dedupe: false,
            search_cache: false,
            anchor_edits: false,
            minimize_output: false,
            route_cheap_tasks: false,
            metrics: true,
            ..TokenEfficiencySettings::default()
        };
        assert!(!settings.is_inert(), "measurement is itself a feature");
    }
}
