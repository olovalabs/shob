//! Session lifecycle: `Active -> Idle -> Hibernated`.
//!
//! The policy is a pure function over an injected `Instant`, so the thresholds
//! and the visibility/work rules are testable without a GPUI context. The
//! application owns the expensive effects: releasing render caches, dropping
//! the terminal grid and spilling its text, and trimming the allocator.
//!
//! Invariants:
//! - A visible session is never idle, whatever its timestamps say.
//! - Work in flight (a command, an agent turn, a queued prompt, an approval)
//!   is never hibernated, so nothing is killed or dropped silently.
//! - Waking is immediate and unconditional; hibernation is the only slow path.

use std::time::{Duration, Instant};

/// How long a quiescent pane waits before it leaves `Active`.
///
/// Zero keeps the pre-lifecycle behaviour: a pane that is not selected releases
/// its disposable render caches on the next status tick. Raising it trades a
/// little memory for less cache churn when switching tabs quickly.
pub const DEFAULT_IDLE_AFTER: Duration = Duration::from_secs(0);

/// How long a pane must be idle *and* quiet before its grid is dropped.
pub const DEFAULT_HIBERNATE_AFTER: Duration = Duration::from_secs(15 * 60);

/// Refuse to hibernate a pane that became quiet before its shell ever ran, and
/// refuse thresholds that would thrash.
const MIN_HIBERNATE_AFTER: Duration = Duration::from_secs(30);
const MAX_HIBERNATE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleState {
    /// Visible, focused, or running work.
    Active,
    /// Not visible and quiet, but still holding its in-memory state.
    Idle,
    /// Grid, render caches and view entities dropped; only a stub remains.
    Hibernated,
}

impl LifecycleState {
    pub fn is_hibernated(self) -> bool {
        self == Self::Hibernated
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    None,
    /// Resources that a foreground render can rebuild should be released.
    ToIdle,
    /// Persist the pane's text, then drop the grid and every view cache.
    ToHibernated,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LifecyclePolicy {
    pub enabled: bool,
    pub idle_after: Duration,
    pub hibernate_after: Duration,
}

impl Default for LifecyclePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_after: DEFAULT_IDLE_AFTER,
            hibernate_after: DEFAULT_HIBERNATE_AFTER,
        }
    }
}

impl LifecyclePolicy {
    /// Clamp a user-supplied policy into a range that cannot thrash. A
    /// hibernation threshold below [`MIN_HIBERNATE_AFTER`] would drop a grid the
    /// person is still reading; one above a day is indistinguishable from off.
    pub fn new(enabled: bool, idle_after: Duration, hibernate_after: Duration) -> Self {
        let idle_after = idle_after.min(MAX_HIBERNATE_AFTER);
        let hibernate_after = hibernate_after
            .clamp(MIN_HIBERNATE_AFTER, MAX_HIBERNATE_AFTER)
            .max(idle_after);
        Self {
            enabled,
            idle_after,
            hibernate_after,
        }
    }

    /// Build from settings and let `SHOB_IDLE_SECS`, `SHOB_HIBERNATE_SECS` and
    /// `SHOB_DISABLE_HIBERNATION` override them for a single run. Taking a
    /// lookup keeps tests independent of process-global environment state.
    pub fn from_lookup(
        mut get: impl FnMut(&str) -> Option<String>,
        idle_after: Duration,
        hibernate_after: Duration,
    ) -> Self {
        let enabled = get("SHOB_DISABLE_HIBERNATION").is_none_or(|value| {
            let value = value.trim().to_ascii_lowercase();
            value == "0" || value == "false" || value == "off"
        });
        let seconds = |key: &str, fallback: Duration| {
            get(key)
                .and_then(|value| value.trim().parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(fallback)
        };
        Self::new(
            enabled,
            seconds("SHOB_IDLE_SECS", idle_after),
            seconds("SHOB_HIBERNATE_SECS", hibernate_after),
        )
    }
}

/// Per-pane timestamps. `last_activity` advances on PTY output, user input, and
/// focus; it is the clock hibernation measures.
#[derive(Clone, Copy, Debug)]
pub struct PaneLifecycle {
    state: LifecycleState,
    last_activity: Instant,
    /// When the pane last rendered, if it ever has. A pane that has never been
    /// shown must not be hibernated before its shell has produced anything.
    last_seen: Option<Instant>,
}

impl PaneLifecycle {
    pub fn new(now: Instant) -> Self {
        Self {
            state: LifecycleState::Active,
            last_activity: now,
            last_seen: None,
        }
    }

    pub fn state(&self) -> LifecycleState {
        self.state
    }

    /// Record real work: PTY output, input, a prompt, or focus.
    pub fn touch(&mut self, now: Instant) {
        self.last_activity = now;
        if self.state != LifecycleState::Hibernated {
            self.state = LifecycleState::Active;
        }
    }

    /// Record that the person actually looked at this pane.
    pub fn mark_seen(&mut self, now: Instant) {
        self.last_seen = Some(now);
        self.touch(now);
    }

    /// A pane that has never rendered has nothing worth reclaiming, and one
    /// that just woke must stay awake for at least one poll interval.
    pub fn can_hibernate(&self) -> bool {
        self.last_seen.is_some()
    }

    /// Decide this tick. `eligible` means the pane is not visible and has no
    /// work in flight; callers compute that from live state, not from age.
    pub fn evaluate(
        &mut self,
        now: Instant,
        policy: LifecyclePolicy,
        eligible: bool,
    ) -> Transition {
        // Waking is explicit: the terminal view wakes on its own render and
        // the app calls `woke`. Evaluations never revive a hibernated pane,
        // even though its frozen screen makes it permanently "eligible".
        if self.state == LifecycleState::Hibernated {
            return Transition::None;
        }
        if !eligible {
            self.state = LifecycleState::Active;
            return Transition::None;
        }
        let quiet_for = now.saturating_duration_since(self.last_activity);
        if policy.enabled && self.can_hibernate() && quiet_for >= policy.hibernate_after {
            self.state = LifecycleState::Hibernated;
            return Transition::ToHibernated;
        }
        if self.state == LifecycleState::Active && quiet_for >= policy.idle_after {
            self.state = LifecycleState::Idle;
            return Transition::ToIdle;
        }
        Transition::None
    }

    /// Called by the wake path after the grid has been rebuilt.
    pub fn woke(&mut self, now: Instant) {
        self.state = LifecycleState::Active;
        self.last_activity = now;
    }
}

/// Estimated bytes for one pane or a whole session. Every field is an estimate
/// and says so in its name at the call site; the point is ranking consumers,
/// not accounting to the byte.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemoryUsage {
    /// alacritty grid (scrollback + screen), in-use cells and row overhead.
    pub grid_bytes: usize,
    /// alacritty's spare rows it keeps resident after a shrink (upper bound).
    pub grid_reserved_bytes: usize,
    /// Text kept for a hibernated pane so waking does not touch the disk.
    pub hibernated_text_bytes: usize,
    /// Bytes buffered from the PTY while hibernated (bounded).
    pub backlog_bytes: usize,
    /// Chat transcript text, including tool output.
    pub message_bytes: usize,
    /// Built chat rows and other disposable render caches.
    pub view_cache_bytes: usize,
    pub entities: usize,
    pub tasks: usize,
    pub threads: usize,
}

impl MemoryUsage {
    pub fn total(&self) -> usize {
        self.grid_bytes
            + self.hibernated_text_bytes
            + self.backlog_bytes
            + self.message_bytes
            + self.view_cache_bytes
    }

    pub fn add(&mut self, other: &Self) {
        self.grid_bytes += other.grid_bytes;
        self.grid_reserved_bytes += other.grid_reserved_bytes;
        self.hibernated_text_bytes += other.hibernated_text_bytes;
        self.backlog_bytes += other.backlog_bytes;
        self.message_bytes += other.message_bytes;
        self.view_cache_bytes += other.view_cache_bytes;
        self.entities += other.entities;
        self.tasks += other.tasks;
        self.threads += other.threads;
    }
}

/// Totals across every pane in the workspace, for the diagnostics line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceMemory {
    pub panes: usize,
    pub active: usize,
    pub idle: usize,
    pub hibernated: usize,
    pub usage: MemoryUsage,
}

impl WorkspaceMemory {
    pub fn record(&mut self, state: LifecycleState, usage: &MemoryUsage) {
        self.panes += 1;
        match state {
            LifecycleState::Active => self.active += 1,
            LifecycleState::Idle => self.idle += 1,
            LifecycleState::Hibernated => self.hibernated += 1,
        }
        self.usage.add(usage);
    }

    /// One compact line, appended to the existing `SHOB_DIAGNOSTICS` report.
    pub fn line(&self) -> String {
        format!(
            "mem panes={} active={} idle={} hibernated={} grid={} reserved={} text={} backlog={} messages={} caches={} total={} threads={} entities={}",
            self.panes,
            self.active,
            self.idle,
            self.hibernated,
            format_bytes(self.usage.grid_bytes),
            format_bytes(self.usage.grid_reserved_bytes),
            format_bytes(self.usage.hibernated_text_bytes),
            format_bytes(self.usage.backlog_bytes),
            format_bytes(self.usage.message_bytes),
            format_bytes(self.usage.view_cache_bytes),
            format_bytes(self.usage.total()),
            self.usage.threads,
            self.usage.entities,
        )
    }
}

/// Human units for the diagnostics line. Binary units, one decimal.
pub fn format_bytes(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    let bytes = bytes as f64;
    if bytes < KB {
        format!("{bytes:.0}B")
    } else if bytes < KB * KB {
        format!("{:.1}K", bytes / KB)
    } else if bytes < KB * KB * KB {
        format!("{:.1}M", bytes / (KB * KB))
    } else {
        format!("{:.2}G", bytes / (KB * KB * KB))
    }
}

/// Return freed pages to the OS after dropping a grid. glibc keeps large freed
/// blocks in its arenas, so RSS otherwise stays at the high-water mark even
/// though the memory is reusable; `malloc_trim` is the only portable way to ask.
///
/// Cheap to call when nothing was freed, and a no-op off Linux/glibc.
pub fn return_free_pages() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn visible_or_busy_panes_never_hibernate() {
        let now = Instant::now();
        let policy = LifecyclePolicy::new(true, Duration::ZERO, Duration::from_secs(60));
        let mut pane = PaneLifecycle::new(now);
        pane.mark_seen(now);
        for step in 1..100 {
            assert_eq!(
                pane.evaluate(at(now, step * 60), policy, false),
                Transition::None
            );
            assert_eq!(pane.state(), LifecycleState::Active);
        }
    }

    #[test]
    fn idle_then_hibernate_then_immediate_wake() {
        let now = Instant::now();
        let policy = LifecyclePolicy::new(true, Duration::ZERO, Duration::from_secs(600));
        let mut pane = PaneLifecycle::new(now);
        pane.mark_seen(now);

        assert_eq!(
            pane.evaluate(at(now, 1), policy, true),
            Transition::ToIdle
        );
        assert_eq!(pane.state(), LifecycleState::Idle);
        assert_eq!(
            pane.evaluate(at(now, 599), policy, true),
            Transition::None
        );
        assert_eq!(
            pane.evaluate(at(now, 600), policy, true),
            Transition::ToHibernated
        );
        assert_eq!(pane.state(), LifecycleState::Hibernated);

        // Once asleep it stays asleep under evaluation; the wake path is the
        // terminal's render plus `woke`, never a timer tick.
        assert_eq!(
            pane.evaluate(at(now, 10_000), policy, false),
            Transition::None
        );
        assert_eq!(pane.state(), LifecycleState::Hibernated);
        pane.woke(at(now, 10_000));
        assert_eq!(pane.state(), LifecycleState::Active);
    }

    #[test]
    fn activity_restarts_the_hibernation_clock() {
        let now = Instant::now();
        let policy = LifecyclePolicy::new(true, Duration::ZERO, Duration::from_secs(600));
        let mut pane = PaneLifecycle::new(now);
        pane.mark_seen(now);
        pane.evaluate(at(now, 1), policy, true);
        pane.touch(at(now, 500));
        assert_eq!(
            pane.evaluate(at(now, 1_099), policy, true),
            Transition::None
        );
        assert_eq!(
            pane.evaluate(at(now, 1_100), policy, true),
            Transition::ToHibernated
        );
    }

    #[test]
    fn a_pane_that_never_rendered_keeps_its_state() {
        let now = Instant::now();
        let policy = LifecyclePolicy::new(true, Duration::ZERO, Duration::from_secs(30));
        let mut pane = PaneLifecycle::new(now);
        assert_eq!(
            pane.evaluate(at(now, 3_600), policy, true),
            Transition::ToIdle
        );
        assert_eq!(pane.state(), LifecycleState::Idle);
    }

    #[test]
    fn disabled_policy_releases_caches_but_never_hibernates() {
        let now = Instant::now();
        let policy = LifecyclePolicy::new(false, Duration::ZERO, Duration::from_secs(60));
        let mut pane = PaneLifecycle::new(now);
        pane.mark_seen(now);
        assert_eq!(
            pane.evaluate(at(now, 10), policy, true),
            Transition::ToIdle
        );
        assert_eq!(
            pane.evaluate(at(now, 86_400), policy, true),
            Transition::None
        );
        assert_eq!(pane.state(), LifecycleState::Idle);
    }

    #[test]
    fn policy_clamps_thresholds_so_it_cannot_thrash() {
        let policy = LifecyclePolicy::new(
            true,
            Duration::from_secs(7_200),
            Duration::from_secs(1),
        );
        assert!(policy.hibernate_after >= policy.idle_after);
        assert!(policy.hibernate_after >= MIN_HIBERNATE_AFTER);

        let policy = LifecyclePolicy::new(
            true,
            Duration::ZERO,
            Duration::from_secs(u32::MAX as u64),
        );
        assert_eq!(policy.hibernate_after, MAX_HIBERNATE_AFTER);
    }

    #[test]
    fn environment_overrides_are_honoured() {
        let policy = LifecyclePolicy::from_lookup(
            |key| match key {
                "SHOB_IDLE_SECS" => Some("5".into()),
                "SHOB_HIBERNATE_SECS" => Some("120".into()),
                "SHOB_DISABLE_HIBERNATION" => Some("1".into()),
                _ => None,
            },
            DEFAULT_IDLE_AFTER,
            DEFAULT_HIBERNATE_AFTER,
        );
        assert!(!policy.enabled);
        assert_eq!(policy.idle_after, Duration::from_secs(5));
        assert_eq!(policy.hibernate_after, Duration::from_secs(120));

        let policy = LifecyclePolicy::from_lookup(
            |key| (key == "SHOB_DISABLE_HIBERNATION").then(|| "0".into()),
            DEFAULT_IDLE_AFTER,
            DEFAULT_HIBERNATE_AFTER,
        );
        assert!(policy.enabled);
    }

    #[test]
    fn memory_totals_rank_grid_before_caches() {
        let mut usage = MemoryUsage {
            grid_bytes: 40 * 1024 * 1024,
            message_bytes: 2 * 1024 * 1024,
            view_cache_bytes: 512 * 1024,
            ..MemoryUsage::default()
        };
        usage.add(&MemoryUsage {
            grid_reserved_bytes: 4 * 1024 * 1024,
            threads: 2,
            ..MemoryUsage::default()
        });
        assert_eq!(usage.total(), 42 * 1024 * 1024 + 512 * 1024);
        assert_eq!(usage.threads, 2);

        let mut workspace = WorkspaceMemory::default();
        workspace.record(LifecycleState::Active, &usage);
        workspace.record(LifecycleState::Hibernated, &MemoryUsage::default());
        assert_eq!(workspace.panes, 2);
        assert_eq!(workspace.active, 1);
        assert_eq!(workspace.hibernated, 1);
        let line = workspace.line();
        assert!(line.contains("hibernated=1"), "{line}");
        assert!(line.contains("total=42.5M"), "{line}");
    }

    #[test]
    fn byte_format_uses_binary_units() {
        assert_eq!(format_bytes(999), "999B");
        assert_eq!(format_bytes(1024), "1.0K");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0M");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.00G");
    }
}
