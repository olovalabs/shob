//! Kathamo attention rules on top of the detected status.
//!
//! The detector in [`crate::status`] answers *what the terminal is doing*. This
//! module answers *what the person needs to know about it*:
//!
//! * `blocked > done > working > idle` priority;
//! * a finish the person did not watch stays `done` until they look, and clears
//!   to `idle` the moment they do;
//! * needing the person always alerts, finishing alerts only if they were away.
//!
//! `Error` counts as blocked and `Unknown`/`Active` count as idle for ranking and
//! alerts, while the richer status stays visible in the pane and session badge.

use crate::status::SessionStatus;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertKind {
    Blocked,
    Done,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Attention {
    /// Whether the person has seen it since it last stopped working.
    pub seen: bool,
    /// Bumped on every finalized change, for "most recent" ordering.
    pub seq: u64,
    /// When the finalized state last changed, ms since the epoch.
    pub since: u64,
    /// The previous finalized status, for transition detection and the latch.
    pub last: Option<SessionStatus>,
}

#[derive(Clone, Debug)]
pub struct SubAgent {
    pub id: String,
    pub title: Option<String>,
    pub status: SessionStatus,
    pub since: u64,
    pub seq: u64,
}

#[derive(Clone, Debug)]
pub struct AppAlert {
    pub kind: AlertKind,
    pub pane_id: usize,
    pub name: String,
    pub place: String,
    /// Looking at it already: sound still plays, the toast is suppressed.
    pub looking: bool,
}

fn busy(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Working | SessionStatus::Blocked | SessionStatus::Error
    )
}

pub struct Resolved {
    pub status: SessionStatus,
}

/// Finalize one pane for this tick.
///
/// `detected` is what the existing poller produced (exit/native/OSC/rule plus
/// the completion latch). `looking` is visible-and-window-active, `focused` is
/// keyboard focus. An agent that was busy and then reports idle/active counts as
/// finished, so a status drop without an explicit `done` still becomes `done`
/// while the person was away.
pub fn resolve(
    attention: &mut Attention,
    detected: SessionStatus,
    is_agent: bool,
    focused: bool,
    looking: bool,
    now_ms: u64,
    seq_counter: &mut u64,
) -> Resolved {
    let idle_like = if focused {
        SessionStatus::Active
    } else {
        SessionStatus::Idle
    };
    let finished = detected == SessionStatus::Done
        || (is_agent
            && attention.last.is_some_and(busy)
            && matches!(detected, SessionStatus::Idle | SessionStatus::Active));

    let next = if busy(detected) {
        // Live agent: the person has seen it, and it is not "done" news.
        attention.seen = true;
        detected
    } else if finished {
        if looking {
            // Finished while they were looking: never show `done`.
            attention.seen = true;
            idle_like
        } else {
            attention.seen = false;
            SessionStatus::Done
        }
    } else if attention.last == Some(SessionStatus::Done) && !attention.seen {
        // A latched finish clears only when looked at; new work resets it.
        if looking {
            attention.seen = true;
            idle_like
        } else {
            SessionStatus::Done
        }
    } else {
        detected
    };

    let changed = attention.last != Some(next);
    if changed {
        *seq_counter += 1;
        attention.seq = *seq_counter;
        attention.since = now_ms;
    }
    attention.last = Some(next);
    Resolved { status: next }
}

/// Kathamo's alert rule for one finalized transition.
pub fn alert_for(
    prev: Option<SessionStatus>,
    next: SessionStatus,
    looking: bool,
) -> Option<AlertKind> {
    let was_blocked = matches!(prev, Some(SessionStatus::Blocked | SessionStatus::Error));
    let is_blocked = matches!(next, SessionStatus::Blocked | SessionStatus::Error);
    let was_busy = prev.is_some_and(busy);
    if is_blocked && !was_blocked {
        Some(AlertKind::Blocked)
    } else if next == SessionStatus::Done && was_busy && !looking {
        Some(AlertKind::Done)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(
        attention: &mut Attention,
        detected: SessionStatus,
        is_agent: bool,
        focused: bool,
        looking: bool,
        now: u64,
        seq: &mut u64,
    ) -> SessionStatus {
        resolve(attention, detected, is_agent, focused, looking, now, seq).status
    }

    #[test]
    fn a_finish_while_away_becomes_done_and_clears_when_looked_at() {
        let mut a = Attention::default();
        let mut seq = 0;
        assert_eq!(
            run(
                &mut a,
                SessionStatus::Working,
                true,
                false,
                true,
                1,
                &mut seq
            ),
            SessionStatus::Working
        );
        assert_eq!(
            run(&mut a, SessionStatus::Idle, true, false, false, 2, &mut seq),
            SessionStatus::Done
        );
        assert!(!a.seen, "an unseen finish is done");
        assert_eq!(
            run(&mut a, SessionStatus::Idle, true, false, true, 3, &mut seq),
            SessionStatus::Idle
        );
        assert!(a.seen, "looking clears done");
    }

    #[test]
    fn an_explicit_done_while_looking_is_idle_not_done() {
        let mut a = Attention::default();
        let mut seq = 0;
        assert_eq!(
            run(
                &mut a,
                SessionStatus::Working,
                true,
                true,
                true,
                1,
                &mut seq
            ),
            SessionStatus::Working
        );
        assert_eq!(
            run(&mut a, SessionStatus::Done, true, true, true, 2, &mut seq),
            SessionStatus::Active
        );
        assert_eq!(
            run(&mut a, SessionStatus::Done, true, false, true, 3, &mut seq),
            SessionStatus::Idle
        );
    }

    #[test]
    fn a_done_latches_until_seen() {
        let mut a = Attention::default();
        let mut seq = 0;
        run(
            &mut a,
            SessionStatus::Working,
            true,
            false,
            false,
            1,
            &mut seq,
        );
        assert_eq!(
            run(&mut a, SessionStatus::Done, true, false, false, 2, &mut seq),
            SessionStatus::Done
        );
        assert_eq!(
            run(&mut a, SessionStatus::Idle, true, false, false, 3, &mut seq),
            SessionStatus::Done
        );
        assert_eq!(
            run(
                &mut a,
                SessionStatus::Unknown,
                true,
                false,
                false,
                4,
                &mut seq
            ),
            SessionStatus::Done
        );
    }

    #[test]
    fn error_stays_error_when_looked_at() {
        let mut a = Attention::default();
        let mut seq = 0;
        assert_eq!(
            run(&mut a, SessionStatus::Error, true, false, true, 1, &mut seq),
            SessionStatus::Error
        );
        assert_eq!(
            run(&mut a, SessionStatus::Error, true, false, true, 2, &mut seq),
            SessionStatus::Error
        );
        assert!(a.seen);
    }

    #[test]
    fn alerts_fire_on_blocked_and_on_done_only_when_away() {
        assert_eq!(
            alert_for(Some(SessionStatus::Working), SessionStatus::Blocked, true),
            Some(AlertKind::Blocked)
        );
        assert_eq!(
            alert_for(Some(SessionStatus::Working), SessionStatus::Done, false),
            Some(AlertKind::Done)
        );
        assert_eq!(
            alert_for(Some(SessionStatus::Working), SessionStatus::Done, true),
            None
        );
        assert_eq!(
            alert_for(None, SessionStatus::Blocked, false),
            Some(AlertKind::Blocked)
        );
        assert_eq!(alert_for(None, SessionStatus::Working, false), None);
    }
}
