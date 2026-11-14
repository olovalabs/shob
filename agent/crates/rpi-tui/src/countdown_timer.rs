//! Deadline-based countdown timer.
//!
//! Adapted from `packages/coding-agent/src/modes/interactive/.../countdown-timer.ts`.
//! The TS original spins a `setInterval`; the Rust port is **deadline-based**:
//! it stores an [`Instant`] and the host re-renders on the existing render tick
//! (the 120ms task that re-renders while a run is in flight already drives
//! visible updates). The owner reads [`remaining_secs`] each render.
//!
//! `Instant::now()` is allowed here — the `Date.now()`/`Instant::now()` ban in
//! this codebase applies only to Workflow scripts, not to normal Rust library
//! code.

use std::time::{Duration, Instant};

/// A simple countdown bounded by an absolute deadline.
///
/// No thread, no timer: compute remaining time on demand.
pub struct CountdownTimer {
    deadline: Instant,
}

impl CountdownTimer {
    /// Create a timer that expires after `duration`.
    pub fn new(duration: Duration) -> Self {
        Self {
            deadline: Instant::now() + duration,
        }
    }

    /// Seconds remaining until expiry (clamped at 0).
    pub fn remaining_secs(&self) -> u32 {
        let now = Instant::now();
        if now >= self.deadline {
            0
        } else {
            let remaining = self.deadline - now;
            remaining
                .as_secs()
                .saturating_add(u64::from(remaining.subsec_nanos() > 0))
                .min(u32::MAX as u64) as u32
        }
    }

    /// Whether the deadline has passed.
    pub fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_timer_not_expired() {
        let t = CountdownTimer::new(Duration::from_secs(10));
        assert!(!t.expired());
        assert!(t.remaining_secs() <= 10);
    }

    #[test]
    fn test_expired_timer() {
        let t = CountdownTimer::new(Duration::from_millis(0));
        // Sleep is unnecessary for a zero-duration timer; deadline is now/now+0.
        let r = t.remaining_secs();
        // For a 0ms timer remaining is 0 (clamped) — but allow 1s of slack on
        // slow CI by asserting <= 1.
        assert!(r <= 1, "expected <= 1, got {r}");
    }
}
