//! Run-level budget for the agent loop.
//!
//! # Why this exists
//!
//! The loop's only exit condition is "the model stopped asking for tools"
//! (`agent_loop.rs`: `while has_more_tool_calls || !pending_messages.is_empty()`).
//! Nothing bounds how long that takes, so a model that keeps emitting tool
//! calls runs until it decides to stop. One observed session did **112 turns in
//! a single run over 867 seconds** (`docs/debugging/llm-repetition-forensics.md` §二), and
//! the same run also paid an O(n²) per-delta clone cost (§4.7) — so the whole
//! 14 minutes was spent on a run the user had no way to interrupt or bound.
//!
//! This is the backstop, not the cure. The re-plan behaviour itself is fixed at
//! the source by keeping plan state in the visible channel (see the system
//! prompt's working-state rule and §8.8 of the forensics doc); this guard only
//! makes sure that when it *does* happen the run ends on a known budget instead
//! of running away.
//!
//! # Off by default (upstream parity)
//!
//! Upstream's loop has no turn/token ceiling at all — it runs until the model
//! stops asking for tools, an abort, or an error. `rpi` therefore ships the
//! guard **disabled** so default behaviour is identical to native, and you opt
//! in per process:
//!
//! ```text
//! RPI_MAX_TURNS_PER_RUN=120 rpi      # stop a single run after 120 turns
//! ```
//!
//! Unset, empty, non-numeric or `0` all mean "no ceiling", exactly like native.
//!
//! # Why only a turn cap
//!
//! A "no progress" detector was measured against the real session and rejected:
//! the longest run of consecutive turns without a successful write was **40**,
//! but those turns were legitimate exploration (nothing was duplicated — only
//! 3 of 148 tool calls were exact repeats). Any stall threshold low enough to
//! catch that would truncate ordinary work, so a single generous turn ceiling is
//! the only signal that is both simple and safe.

/// Why a run was stopped by the guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetStop {
    /// The run reached its turn ceiling.
    TurnLimit { turns: u32, limit: u32 },
}

impl BudgetStop {
    /// One-line explanation, surfaced to the user and recorded in the
    /// transcript so a truncated run is never mistaken for a finished one.
    pub fn message(&self) -> String {
        match self {
            Self::TurnLimit { turns, limit } => format!(
                "Stopped after {turns} turns in a single run (limit {limit}). \
                 The task may be unfinished — send a follow-up to continue."
            ),
        }
    }
}

/// Suggested turn ceiling when the guard is switched on.
///
/// Chosen well above real work but far below a runaway: the observed failing
/// session needed 112 turns to do very little, while a large refactor
/// legitimately uses a few dozen. Not applied unless the operator enables the
/// guard — see [`RunBudget::from_env`]. `0` means unlimited.
pub const DEFAULT_MAX_TURNS_PER_RUN: u32 = 120;

/// Environment variable that switches the guard on: `RPI_MAX_TURNS_PER_RUN=<n>`
/// (`n > 0`). Anything else — unset, empty, non-numeric, `0` — leaves the run
/// unbounded, matching upstream.
pub const MAX_TURNS_ENV: &str = "RPI_MAX_TURNS_PER_RUN";

// Readers asserting environment stability and tests mutating this key must
// share one lock. Tokio's guard is safe to retain across child-run awaits.
#[cfg(test)]
pub(crate) static MAX_TURNS_ENV_TEST_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

/// Counts turns in one run and reports when the budget is exhausted.
///
/// The harness builds this once per run and feeds it every turn's result via
/// the `should_stop_after_turn` hook.
#[derive(Debug, Clone)]
pub struct RunBudget {
    turns: u32,
    max_turns: u32,
    /// The stop that fired, if the budget was exhausted this run. Read after
    /// the loop so the harness can record *why* the run ended.
    stop: Option<BudgetStop>,
}

impl Default for RunBudget {
    /// Disabled: upstream has no run-level ceiling, so neither does `rpi`
    /// unless the operator asks for one (see [`RunBudget::from_env`]).
    fn default() -> Self {
        Self::new(0)
    }
}

impl RunBudget {
    /// Create a budget allowing `max_turns` turns (`0` disables the guard).
    pub fn new(max_turns: u32) -> Self {
        Self {
            turns: 0,
            max_turns,
            stop: None,
        }
    }

    /// Build from the environment: [`MAX_TURNS_ENV`] holds the ceiling, and
    /// anything unparsable or non-positive disables the guard (native parity).
    pub fn from_env() -> Self {
        match std::env::var(MAX_TURNS_ENV) {
            Ok(raw) => match raw.trim().parse::<u32>() {
                Ok(n) => Self::new(n),
                Err(_) if raw.trim().is_empty() => Self::new(0),
                Err(_) => {
                    tracing::warn!(
                        value = %raw,
                        "{} is not a number; running without a turn ceiling",
                        MAX_TURNS_ENV
                    );
                    Self::new(0)
                }
            },
            Err(_) => Self::new(0),
        }
    }

    /// Whether the guard is active at all.
    pub fn is_enabled(&self) -> bool {
        self.max_turns > 0
    }

    /// Turns completed so far in this run.
    pub fn turns(&self) -> u32 {
        self.turns
    }

    /// The configured ceiling (`0` = unlimited).
    pub fn max_turns(&self) -> u32 {
        self.max_turns
    }

    /// The stop that ended this run, if the budget was exhausted.
    pub fn stop(&self) -> Option<BudgetStop> {
        self.stop
    }

    /// Record one completed turn and report whether the run must stop.
    ///
    /// Called from `should_stop_after_turn`, i.e. once per assistant turn after
    /// its tool results were applied — the same place the loop would otherwise
    /// decide to continue.
    pub fn observe_turn(&mut self) -> Option<BudgetStop> {
        self.turns = self.turns.saturating_add(1);
        if self.max_turns > 0 && self.turns >= self.max_turns {
            let stop = BudgetStop::TurnLimit {
                turns: self.turns,
                limit: self.max_turns,
            };
            self.stop = Some(stop);
            return Some(stop);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_exactly_at_the_ceiling() {
        let mut budget = RunBudget::new(3);
        assert_eq!(budget.observe_turn(), None);
        assert_eq!(budget.observe_turn(), None);
        assert_eq!(
            budget.observe_turn(),
            Some(BudgetStop::TurnLimit { turns: 3, limit: 3 })
        );
        assert_eq!(budget.turns(), 3);
    }

    #[test]
    fn zero_disables_the_guard() {
        let mut budget = RunBudget::new(0);
        assert!(!budget.is_enabled());
        for _ in 0..5_000 {
            assert_eq!(budget.observe_turn(), None);
        }
        assert_eq!(budget.turns(), 5_000);
    }

    #[test]
    fn default_is_unbounded_like_native_pi() {
        // Upstream's loop has no ceiling, so the default must not invent one:
        // the guard is opt-in (RPI_MAX_TURNS_PER_RUN).
        let mut budget = RunBudget::default();
        assert!(!budget.is_enabled());
        assert_eq!(budget.max_turns(), 0);
        for _ in 0..5_000 {
            assert_eq!(budget.observe_turn(), None);
        }
    }

    #[test]
    fn from_env_enables_only_for_a_positive_number() {
        let _guard = MAX_TURNS_ENV_TEST_LOCK.blocking_lock();
        // Restore even on panic, before releasing the common environment lock.
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match &self.0 {
                    Some(value) => std::env::set_var(MAX_TURNS_ENV, value),
                    None => std::env::remove_var(MAX_TURNS_ENV),
                }
            }
        }
        let _restore = Restore(std::env::var_os(MAX_TURNS_ENV));

        std::env::remove_var(MAX_TURNS_ENV);
        assert!(
            !RunBudget::from_env().is_enabled(),
            "unset must be unbounded"
        );

        std::env::set_var(MAX_TURNS_ENV, "7");
        let budget = RunBudget::from_env();
        assert!(budget.is_enabled());
        assert_eq!(budget.max_turns(), 7);

        for raw in ["0", "", "  ", "on", "abc", "-3"] {
            std::env::set_var(MAX_TURNS_ENV, raw);
            assert!(
                !RunBudget::from_env().is_enabled(),
                "{raw:?} must leave the run unbounded"
            );
        }
    }

    #[test]
    fn stop_retains_the_reason_for_the_post_loop_notice() {
        let mut budget = RunBudget::new(2);
        assert_eq!(budget.stop(), None);
        assert_eq!(budget.observe_turn(), None);
        assert!(budget.observe_turn().is_some());
        // The harness reads this after the loop to record why the run ended.
        assert_eq!(
            budget.stop(),
            Some(BudgetStop::TurnLimit { turns: 2, limit: 2 })
        );
    }

    #[test]
    fn stop_message_names_the_budget_and_says_work_may_remain() {
        let message = BudgetStop::TurnLimit {
            turns: 120,
            limit: 120,
        }
        .message();
        assert!(message.contains("120"), "{message}");
        assert!(message.contains("unfinished"), "{message}");
    }
}
