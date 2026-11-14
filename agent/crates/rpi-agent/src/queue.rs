//! Mirrors the queue portion of `packages/agent/src/agent.ts`
//! (`PendingMessageQueue`) + the `QueueMode` type from
//! `packages/agent/src/types.ts`.
//!
//! The agent loop has two drain points:
//! - **steering**: fired mid-run after a turn's tool batch settles (before the
//!   next LLM call), to inject guidance the app pushed while the model was
//!   working. Drained via [`PendingMessageQueue::try_drain`].
//! - **follow-up**: fired when the loop would otherwise stop (no tool calls +
//!   no steering), to keep the conversation going with queued user messages.
//!
//! Both reuse the same [`PendingMessageQueue`] type with a [`QueueMode`]
//! controlling how many messages a single `try_drain` releases.

use std::collections::VecDeque;

use crate::message::AgentMessage;
use crate::types::QueueMode;

/// A FIFO of `AgentMessage`s waiting to be injected at a drain point. Mirrors
/// TS `PendingMessageQueue`. `Clone` so `Agent` can keep a steering instance
/// and a follow-up instance independently.
#[derive(Debug, Clone, Default)]
pub struct PendingMessageQueue {
    mode: QueueMode,
    pending: VecDeque<AgentMessage>,
}

impl PendingMessageQueue {
    pub fn new(mode: QueueMode) -> Self {
        Self {
            mode,
            pending: VecDeque::new(),
        }
    }

    pub fn mode(&self) -> QueueMode {
        self.mode
    }

    /// Append a message to the tail. Mirrors TS `enqueue`.
    pub fn enqueue(&mut self, message: AgentMessage) {
        self.pending.push_back(message);
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Drain queued messages per [`QueueMode`]:
    /// - `All`: return and remove every queued message, in enqueue order.
    /// - `OneAtATime`: return and remove only the oldest, leaving the rest.
    ///
    /// Returns an empty `Vec` when nothing is queued (the loop treats an empty
    /// drain as "no injection"). Mirrors TS `tryDrain`.
    pub fn try_drain(&mut self) -> Vec<AgentMessage> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        match self.mode {
            QueueMode::All => self.pending.drain(..).collect(),
            QueueMode::OneAtATime => self
                .pending
                .pop_front()
                .map(|m| vec![m])
                .unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ai::types::UserMessage;
    use AgentMessage;

    fn user(s: &str) -> AgentMessage {
        AgentMessage::User(UserMessage::new(s, 0))
    }

    #[test]
    fn drain_all_empties_queue() {
        let mut q = PendingMessageQueue::new(QueueMode::All);
        q.enqueue(user("a"));
        q.enqueue(user("b"));
        let drained = q.try_drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].role().as_str(), "user");
        assert!(q.is_empty());
        assert!(q.try_drain().is_empty());
    }

    #[test]
    fn drain_one_at_a_time_keeps_tail() {
        let mut q = PendingMessageQueue::new(QueueMode::OneAtATime);
        q.enqueue(user("a"));
        q.enqueue(user("b"));
        let first = q.try_drain();
        assert_eq!(first.len(), 1);
        assert_eq!(q.len(), 1);
        let second = q.try_drain();
        assert_eq!(second.len(), 1);
        assert!(q.is_empty());
    }

    #[test]
    fn drain_empty_returns_empty() {
        let mut q = PendingMessageQueue::new(QueueMode::All);
        assert!(q.try_drain().is_empty());
    }
}
