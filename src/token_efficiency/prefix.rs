//! Prompt-prefix stability guard.
//!
//! Provider prompt caching is a prefix cache: the moment anything before the
//! last breakpoint changes, every token after it is re-billed at the input rate
//! and, worse, paid again at the write rate. The two ways that happens silently
//! are a system prompt that embeds something volatile (a timestamp, a cwd, a
//! random id) and a tool list that reorders.
//!
//! `agent/crates/harness/src/system_prompt.rs::compose_system_prompt` already
//! composes in the stable order (base → append → context → skills), which puts
//! the volatile project-context section last. This module does not change that;
//! it watches it. A digest over `(system prompt, sorted tool names)` is recorded
//! per turn, and a change between two turns without a tool-set change is
//! reported as drift so the metrics file can attribute a bad cache hit rate to
//! its cause instead of leaving it a mystery.

use super::hash::{Digest, key_digest};

/// The cacheable part of a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixSnapshot {
    /// Digest of the system prompt exactly as it will be sent.
    pub system: Digest,
    /// Tool names, sorted. Sorted because the provider caches the tool block;
    /// order is irrelevant to the model and must not be a source of drift.
    pub tool_names: Vec<String>,
    /// Digest over (system, sorted tools).
    pub digest: Digest,
}

impl PrefixSnapshot {
    pub fn new(system: &str, tool_names: &[String]) -> Self {
        let mut sorted: Vec<String> = tool_names.to_vec();
        sorted.sort();
        sorted.dedup();
        let system_digest = Digest::from_bytes(system.as_bytes());
        let mut parts: Vec<&str> = vec![&system_digest.hex()];
        parts.extend(sorted.iter().map(String::as_str));
        let digest = Digest::from_bytes(key_digest(&parts).as_bytes());
        Self {
            system: system_digest,
            tool_names: sorted,
            digest,
        }
    }
}

/// What changed between two snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drift {
    /// Identical.
    None,
    /// The tool set changed (added or removed a tool). Expected during
    /// interaction; costs one cache write.
    ToolSetChanged,
    /// The system prompt changed. This is the expensive one: it invalidates the
    /// tool block too, because tools are cached after the system prompt.
    SystemChanged,
    /// Both.
    Both,
}

impl Drift {
    /// True when the change was not explainable by a deliberate tool-set edit,
    /// i.e. something volatile leaked into the prefix.
    pub fn is_suspicious(self) -> bool {
        matches!(self, Drift::SystemChanged | Drift::Both)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Drift::None => "none",
            Drift::ToolSetChanged => "tool_set_changed",
            Drift::SystemChanged => "system_changed",
            Drift::Both => "both_changed",
        }
    }
}

/// Compares successive snapshots.
#[derive(Default)]
pub struct PrefixGuard {
    previous: Option<PrefixSnapshot>,
    drift_count: u64,
}

impl PrefixGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `snapshot` and report what changed since the last one.
    pub fn observe(&mut self, snapshot: PrefixSnapshot) -> Drift {
        let drift = match &self.previous {
            None => Drift::None,
            Some(previous) => {
                if previous.digest == snapshot.digest {
                    Drift::None
                } else {
                    let system_changed = previous.system != snapshot.system;
                    let tools_changed = previous.tool_names != snapshot.tool_names;
                    match (system_changed, tools_changed) {
                        (true, true) => Drift::Both,
                        (true, false) => Drift::SystemChanged,
                        (false, true) => Drift::ToolSetChanged,
                        (false, false) => Drift::None,
                    }
                }
            }
        };
        if drift.is_suspicious() {
            self.drift_count += 1;
        }
        self.previous = Some(snapshot);
        drift
    }

    pub fn drift_count(&self) -> u64 {
        self.drift_count
    }

    /// Forget the previous snapshot, e.g. after a deliberate resource reload, so
    /// the next observation is not reported as drift.
    pub fn reset(&mut self) {
        self.previous = None;
    }
}

/// Checks that a composed system prompt does not embed obvious volatility.
///
/// This is a lint, not a parser: it looks for the shapes that have actually
/// caused this failure (epoch timestamps, absolute dates, random ids) and stays
/// quiet about project context a user legitimately wants in the prompt. Call it
/// from a test with the real prompt, or once at startup under
/// `SHOB_TOKEN_EFFICIENCY=debug`.
pub fn volatile_markers(system_prompt: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut epoch_like = 0usize;
    let mut word = String::new();

    for character in system_prompt.chars() {
        if character.is_ascii_digit() {
            word.push(character);
            continue;
        }
        // A run of 10+ digits is an epoch timestamp, not a line number.
        if word.len() >= 10 {
            epoch_like += 1;
        }
        word.clear();
    }
    if word.len() >= 10 {
        epoch_like += 1;
    }
    if epoch_like > 0 {
        found.push(format!(
            "{epoch_like} long digit run(s) that look like embedded timestamps"
        ));
    }

    for marker in ["Current time:", "Current date:", "UTC", "localhost:"] {
        if system_prompt.contains(marker) {
            found.push(format!("literal {marker:?}"));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn an_identical_prefix_reports_no_drift() {
        let mut guard = PrefixGuard::new();
        let snapshot = PrefixSnapshot::new("You are Shob.", &tools(&["read", "bash"]));
        assert_eq!(guard.observe(snapshot.clone()), Drift::None);
        assert_eq!(guard.observe(snapshot), Drift::None);
        assert_eq!(guard.drift_count(), 0);
    }

    #[test]
    fn tool_order_alone_is_not_drift() {
        let mut guard = PrefixGuard::new();
        guard.observe(PrefixSnapshot::new("sys", &tools(&["read", "bash", "grep"])));
        let reordered = PrefixSnapshot::new("sys", &tools(&["grep", "bash", "read"]));
        assert_eq!(guard.observe(reordered), Drift::None, "tool order must not invalidate the cache");
    }

    #[test]
    fn duplicate_tool_names_do_not_change_the_digest() {
        let mut guard = PrefixGuard::new();
        guard.observe(PrefixSnapshot::new("sys", &tools(&["read", "bash"])));
        assert_eq!(
            guard.observe(PrefixSnapshot::new("sys", &tools(&["bash", "read", "read"]))),
            Drift::None
        );
    }

    #[test]
    fn adding_a_tool_is_an_expected_change() {
        let mut guard = PrefixGuard::new();
        guard.observe(PrefixSnapshot::new("sys", &tools(&["read"])));
        assert_eq!(
            guard.observe(PrefixSnapshot::new("sys", &tools(&["read", "write"]))),
            Drift::ToolSetChanged
        );
        assert_eq!(guard.drift_count(), 0, "a deliberate tool edit is not suspected drift");
    }

    #[test]
    fn a_changed_system_prompt_is_flagged_as_suspicious() {
        let mut guard = PrefixGuard::new();
        guard.observe(PrefixSnapshot::new("You are Shob.", &tools(&["read"])));
        assert_eq!(
            guard.observe(PrefixSnapshot::new("You are Shob. It is 12:00.", &tools(&["read"]))),
            Drift::SystemChanged
        );
        assert_eq!(guard.drift_count(), 1);
        assert!(Drift::SystemChanged.is_suspicious());
    }

    #[test]
    fn both_changing_is_reported() {
        let mut guard = PrefixGuard::new();
        guard.observe(PrefixSnapshot::new("a", &tools(&["read"])));
        assert_eq!(
            guard.observe(PrefixSnapshot::new("b", &tools(&["read", "grep"]))),
            Drift::Both
        );
    }

    #[test]
    fn reset_suppresses_the_next_comparison() {
        let mut guard = PrefixGuard::new();
        guard.observe(PrefixSnapshot::new("old", &tools(&["read"])));
        guard.reset();
        assert_eq!(
            guard.observe(PrefixSnapshot::new("new", &tools(&["write"]))),
            Drift::None,
            "a deliberate reload is not drift"
        );
        assert_eq!(guard.drift_count(), 0);
    }

    #[test]
    fn a_timestamp_in_the_system_prompt_is_detected() {
        let clean = "You are Shob. Read AGENTS.md.";
        assert!(volatile_markers(clean).is_empty());

        let stamped = "You are Shob.\nSession started 1735689600123 ms ago.";
        let markers = volatile_markers(stamped);
        assert!(!markers.is_empty(), "an embedded epoch must be reported");
        assert!(markers[0].contains("timestamp"));
    }

    #[test]
    fn ordinary_numbers_are_not_false_positives() {
        let prompt = "Use 128 KiB frames, 1 ms coalescing, and 7501 for OSC.";
        assert!(volatile_markers(prompt).is_empty());
    }

    #[test]
    fn drift_labels_are_stable_strings() {
        assert_eq!(Drift::None.as_str(), "none");
        assert_eq!(Drift::ToolSetChanged.as_str(), "tool_set_changed");
        assert_eq!(Drift::SystemChanged.as_str(), "system_changed");
    }
}
