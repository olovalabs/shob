//! Follow-up + one-at-a-time steering queue behavior (plan §2.3 / `queue.rs`).
//!
//! Mirrors TS `agent.test.ts`:
//! - "continue() should process queued follow-up messages after an assistant turn"
//! - "continue() should keep one-at-a-time steering semantics from assistant tail"
//!
//! The low-level loop exposes the follow-up/steering queue mechanics via the
//! `AgentLoopConfig` hooks `get_follow_up_messages` / `get_steering_messages`.
//! `Agent::continue_run` is a thin wrapper over `run_agent_loop_continue`; its
//! queue behavior IS the loop's queue behavior, so these tests exercise the loop
//! directly with the same hook shapes an `Agent` installs.
//!
//! - **Follow-up**: after a turn that would otherwise stop (no tool calls, no
//!   steering), the loop drains `get_follow_up_messages`; non-empty → inject the
//!   message and run another turn. The assistant reply ends the new messages.
//! - **One-at-a-time steering**: with `QueueMode::OneAtATime`, a single drain
//!   releases only one of two queued steering messages, so one assistant turn
//!   processes Steering-1 and a second processes Steering-2 (2 LLM calls total).

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;

use common::{assistant_text, base_config, user_message};
use rpi_agent::{AgentContext, AgentEvent, GetFollowUpMessages, GetSteeringMessages};
use rpi_ai::types::StopReason;

/// Count `turn_start` events — one per LLM call.
fn turn_start_count(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnStart))
        .count()
}

/// A simple shared FIFO used to model the `Agent`'s follow-up/steering enqueued
/// queue. `follow_up(msg)` / `steer(msg)` push into it; the loop drains it
/// through the hook. `drain_all` mirrors `QueueMode::All`; `drain_one` mirrors
/// `QueueMode::OneAtATime`.
mod queue_state {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    pub struct Queue<T> {
        inner: Arc<Mutex<VecDeque<T>>>,
    }

    impl<T> Queue<T> {
        pub fn new() -> Self {
            Self {
                inner: Arc::new(Mutex::new(VecDeque::new())),
            }
        }
        pub fn push(&self, value: T) {
            self.inner.lock().expect("queue lock").push_back(value);
        }
        pub fn drain_all(&self) -> Vec<T> {
            self.inner.lock().expect("queue lock").drain(..).collect()
        }
        pub fn drain_one(&self) -> Vec<T> {
            self.inner
                .lock()
                .expect("queue lock")
                .pop_front()
                .map(|m| vec![m])
                .unwrap_or_default()
        }
    }
}

#[tokio::test]
async fn follow_up_queue_drives_an_extra_turn_after_stop() {
    // Mirrors TS "continue() should process queued follow-up messages after an
    // assistant turn". The first turn produces an assistant reply with no tool
    // calls (loop would stop). The follow-up queue holds one "Queued follow-up"
    // user message; the loop drains it and runs a second turn whose assistant
    // reply ends the new messages.
    let follow_ups = queue_state::Queue::new();
    follow_ups.push(user_message("Queued follow-up"));

    // The follow-up hook drains ALL queued messages once; subsequent calls
    // return empty (the queue is now empty).
    let get_follow_up: GetFollowUpMessages = {
        let follow_ups = follow_ups.clone();
        Arc::new(move || {
            let follow_ups = follow_ups.clone();
            Box::pin(async move { follow_ups.drain_all() })
        })
    };

    let mut config = base_config();
    config.get_follow_up_messages = Some(get_follow_up);

    let stream_fn = common::mock_stream_fn(vec![
        assistant_text("Processed 1", StopReason::Stop),
        assistant_text("Processed 2", StopReason::Stop),
    ]);

    let (collector, events_buf) = rpi_agent::CollectorEmitter::new();
    let emit: Arc<dyn rpi_agent::AgentEmitter> = Arc::new(collector);
    let new_messages = rpi_agent::run_agent_loop(
        vec![user_message("Initial")],
        AgentContext::default(),
        config,
        emit,
        stream_fn,
    )
    .await
    .expect("run ok");
    let events = events_buf.lock().expect("events lock").clone();

    // Two turns: the initial + the follow-up-driven turn.
    assert_eq!(
        turn_start_count(&events),
        2,
        "follow-up queue should drive exactly one extra turn (2 total)"
    );

    // The "Queued follow-up" user message is in the new messages, and the last
    // new message is an assistant reply.
    let has_follow_up = new_messages.iter().any(|m| match m {
        rpi_agent::AgentMessage::User(u) => u.content.as_text() == Some("Queued follow-up"),
        _ => false,
    });
    assert!(has_follow_up, "follow-up message should be in new_messages");
    let last = new_messages.last().expect("non-empty new messages");
    assert_eq!(
        last.role().as_str(),
        "assistant",
        "last new message should be the follow-up turn's assistant reply"
    );
}

#[tokio::test]
async fn one_at_a_time_steering_drains_one_message_per_turn() {
    // Mirrors TS "continue() should keep one-at-a-time steering semantics from
    // assistant tail". Two steering messages are queued; each turn drains ONE
    // (OneAtATime), so the run needs exactly 2 LLM calls to process both. The
    // 4 most-recent new messages are [user, assistant, user, assistant].
    let steering = queue_state::Queue::new();
    steering.push(user_message("Steering 1"));
    steering.push(user_message("Steering 2"));

    let get_steering: GetSteeringMessages = {
        let steering = steering.clone();
        Arc::new(move || {
            let steering = steering.clone();
            Box::pin(async move { steering.drain_one() })
        })
    };

    let mut config = base_config();
    config.get_steering_messages = Some(get_steering);

    let stream_fn = common::mock_stream_fn(vec![
        assistant_text("Processed 1", StopReason::Stop),
        assistant_text("Processed 2", StopReason::Stop),
        // A third call would only happen if both messages were drained at once
        // (buggy All-mode) — leaving it out so a miscount fails loudly. The mock
        // exhausts to an Error event if a 3rd call is made.
    ]);

    let (collector, events_buf) = rpi_agent::CollectorEmitter::new();
    let emit: Arc<dyn rpi_agent::AgentEmitter> = Arc::new(collector);
    let new_messages = rpi_agent::run_agent_loop(
        vec![user_message("Initial")],
        AgentContext::default(),
        config,
        emit,
        stream_fn,
    )
    .await
    .expect("run ok");
    let events = events_buf.lock().expect("events lock").clone();

    // Exactly 2 LLM calls — one per drained steering message.
    assert_eq!(
        turn_start_count(&events),
        2,
        "one-at-a-time steering should yield 2 turns (one per message): {events:?}"
    );

    // The 4 most-recent new messages are [user, assistant, user, assistant].
    let recent: Vec<String> = new_messages
        .iter()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|m| m.role().as_str().to_string())
        .collect();
    assert_eq!(
        recent,
        vec!["user", "assistant", "user", "assistant"],
        "recent roles should alternate user/assistant for the two steering turns, got: {recent:?}"
    );
}
