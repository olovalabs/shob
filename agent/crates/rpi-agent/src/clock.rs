//! Wall-clock helpers shared by the agent loop and the `Agent` facade.
//!
//! # Why this module exists
//!
//! Both `agent_loop::create_tool_result_message` and `Agent::prompt` used to
//! stamp their messages from a bare atomic **counter** that returned
//! `1, 2, 3, …`:
//!
//! ```ignore
//! fn now_ms() -> i64 {
//!     static T: AtomicI64 = AtomicI64::new(1);
//!     T.fetch_add(1, Ordering::Relaxed)   // not a clock
//! }
//! ```
//!
//! The comment claimed "the loop only needs ordering + JSONL serializability,
//! not wall-clock accuracy. Uses an atomic counter so tests are deterministic."
//! In practice the counter is only ever *read* — nothing orders by it — so the
//! only observable effect was that every persisted `toolResult` (and every
//! user message from `Agent::prompt`) carried a nonsense `timestamp` in the
//! durable session: `29, 38, 55, 62, …` next to assistant messages stamped with
//! real epoch milliseconds. Anything that trusts timestamps (cache idle
//! detection, exports, `/usage`, third-party readers of the JSONL) was wrong.
//!
//! This replaces the counter with the system clock while keeping the property
//! the counter *did* provide: values never go backwards, so ordering stays
//! well-defined even if the clock steps backwards (NTP correction) or two
//! messages land inside the same millisecond.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Wall-clock milliseconds since the Unix epoch, floored to be strictly
/// increasing.
///
/// Returns real wall-clock time. When the clock repeats within the same
/// millisecond or steps backwards, the previous value is incremented by one
/// instead, so a message produced later never sorts before an earlier one.
pub(crate) fn now_ms() -> i64 {
    /// Highest value handed out so far. `0` means "nothing stamped yet" and is
    /// also the floor for a clock that reports before the epoch.
    static LAST: AtomicI64 = AtomicI64::new(0);

    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0);

    let mut previous = LAST.load(Ordering::Relaxed);
    loop {
        let next = if wall > previous { wall } else { previous + 1 };
        match LAST.compare_exchange_weak(previous, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            // Another thread won; retry against its value so `next` is still
            // strictly greater than everything already handed out.
            Err(actual) => previous = actual,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_real_wall_clock_not_a_counter() {
        // The regression: the old counter started at 1 and returned 1, 2, 3…
        // A real epoch-millisecond value is ~12 orders of magnitude larger.
        let now = now_ms();
        assert!(
            now > 1_600_000_000_000,
            "expected epoch milliseconds, got {now}"
        );
    }

    #[test]
    fn strictly_increasing_within_a_millisecond() {
        let first = now_ms();
        let second = now_ms();
        let third = now_ms();
        assert!(second > first, "{second} !> {first}");
        assert!(third > second, "{third} !> {second}");
    }

    #[test]
    fn concurrent_callers_never_see_a_duplicate_or_a_regression() {
        let handles: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| (0..500).map(|_| now_ms()).collect::<Vec<_>>()))
            .collect();
        let mut all: Vec<i64> = Vec::new();
        for handle in handles {
            all.extend(handle.join().expect("thread"));
        }
        all.sort_unstable();
        let before = all.len();
        all.dedup();
        assert_eq!(all.len(), before, "duplicate timestamps was observed");
    }
}
