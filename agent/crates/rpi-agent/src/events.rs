//! Mirrors `packages/agent/src/types.ts::AgentEvent` — the lifecycle events the
//! agent loop emits. Carried over a `broadcast::Sender<AgentEvent>` so multiple
//! subscribers each get their own copy; payloads are `Arc`-wrapped where they
//! are large to keep broadcast clones cheap.
//!
//! The TS `AgentEvent` is a discriminated union on `type`. We model it as a
//! tagged enum. It is `Debug + Clone` only — `AgentEvent` is not serialized
//! across the wire (session persistence stores `AgentMessage`s, not events);
//! tests compare event sequences via `Debug`. This avoids requiring
//! `Deserialize` on `AssistantMessageEvent` (which is stream-protocol-only).

use rpi_ai::types::{AssistantMessage, AssistantMessageEvent, ToolResultMessage};
use std::sync::Arc;

use crate::message::AgentMessage;
use crate::types::AgentToolResult;

/// An event emitted by the agent loop. Mirrors TS `AgentEvent` (tagged on
/// `type`). `AgentMessage` payloads are owned (cloned) so broadcast subscribers
/// get independent copies — except [`AgentEvent::MessageUpdate`], which carries
/// the streaming partial as a cheap `Arc` because it fires once per delta.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Emitted once at the start of a run, before `turn_start`.
    AgentStart,
    /// Emitted once at the end of a run; carries the new messages produced.
    AgentEnd { messages: Vec<AgentMessage> },
    /// Emitted after a retryable provider failure, before the backoff wait.
    RetryScheduled {
        /// One-based retry number (the initial request is not counted).
        attempt: u32,
        /// Maximum number of retries allowed for this request.
        max_retries: u32,
        /// Backoff duration before the next provider request.
        delay_ms: u64,
        /// Diagnostic from the failed provider request.
        error: String,
    },
    /// Emitted at the start of each turn (a turn = one assistant response + its
    /// tool calls/results).
    TurnStart,
    /// Emitted after a turn's assistant message + tool results settle. Carries
    /// the assistant message (as an `AgentMessage`) and the tool-result
    /// messages produced this turn, in source/ordinal order.
    TurnEnd {
        message: AgentMessage,
        tool_results: Vec<ToolResultMessage>,
        /// How many messages were sitting in the steering + follow-up queues
        /// when this turn settled.
        ///
        /// Consumers use it for a `pendingMessages` indicator ("N queued"):
        /// the UI needs to know "there are still N messages waiting", which is
        /// *not* the same as "a follow-up arrived" (`follow_ups` is drained
        /// before the round decides to continue).
        ///
        /// Additive field: every constructor passes it explicitly; every
        /// consumer that doesn't care uses `..`.
        #[doc(alias = "pendingMessages")]
        pending_messages: usize,
    },
    /// Emitted when any message (user prompt, assistant response, tool result,
    /// custom) is appended to the transcript.
    MessageStart {
        message: AgentMessage,
        message_id: Option<String>,
    },
    /// Emitted only for assistant messages, on each streaming delta. Carries
    /// the underlying `AssistantMessageEvent` plus a **shared** snapshot of the
    /// partial assistant message.
    ///
    /// The snapshot is an `Arc`, not an owned message: a delta used to deep-clone
    /// the whole growing message here, again in the loop's context update, and
    /// again in each consumer — quadratic in the output length. Consumers that
    /// need it as JSON use [`crate::message::assistant_json`] to keep the wire
    /// shape identical without copying.
    MessageUpdate {
        /// Session entry ID assigned by the durable host, absent for direct agents.
        message_id: Option<String>,
        message: Arc<AssistantMessage>,
        assistant_message_event: AssistantMessageEvent,
    },
    /// Emitted when a message finishes (complement of `MessageStart`).
    MessageEnd {
        message: AgentMessage,
        message_id: Option<String>,
    },
    /// Emitted before a tool call starts executing.
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    /// Emitted on a partial tool result pushed via `on_update`.
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
        partial_result: Arc<AgentToolResult>,
    },
    /// Emitted when a tool call finishes (success or error). `is_error` marks
    /// the error path; `result` carries the final tool result.
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
    },
}

impl AgentEvent {
    pub fn type_tag(&self) -> &'static str {
        match self {
            AgentEvent::AgentStart => "agent_start",
            AgentEvent::AgentEnd { .. } => "agent_end",
            AgentEvent::RetryScheduled { .. } => "retry_scheduled",
            AgentEvent::TurnStart => "turn_start",
            AgentEvent::TurnEnd { .. } => "turn_end",
            AgentEvent::MessageStart { .. } => "message_start",
            AgentEvent::MessageUpdate { .. } => "message_update",
            AgentEvent::MessageEnd { .. } => "message_end",
            AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
            AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
            AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
        }
    }

    /// True for the terminal `AgentEnd` event.
    pub fn is_terminal(&self) -> bool {
        matches!(self, AgentEvent::AgentEnd { .. })
    }
}

/// Sink the loop pushes events into. Mirrors TS `AgentEventSink =
/// (event: AgentEvent) => Promise<void> | void`. Implementations:
/// [`CollectorEmitter`] (tests/drain), [`BroadcastEmitter`] (live `Agent`).
///
/// Two emit surfaces:
/// - [`AgentEmitter::emit`] — async, awaited in event order by the loop.
/// - [`AgentEmitter::try_emit`] — sync, non-blocking, used by tool `on_update`
///   callbacks (which are `&dyn Fn` and cannot await). For `CollectorEmitter`
///   this pushes under the mutex; for `BroadcastEmitter` it's `tx.send`.
///   Order is preserved because `try_emit` is only ever called from within a
///   single tool's `execute`, and `emit` is awaited between tool phases.
pub trait AgentEmitter: Send + Sync {
    /// Optional native child observation; ordinary emitters remain unchanged.
    fn tool_program_observer(
        &self,
    ) -> Option<std::sync::Arc<dyn crate::tool_program_observation::ToolProgramObserver>> {
        None
    }

    /// Async emit — awaited by the loop so event order matches emit call order.
    fn emit(&self, event: AgentEvent) -> futures::future::BoxFuture<'static, ()>;

    /// Mark a queue delivery explicitly; hosts may attach its durable reservation.
    /// Unlike ordinary generation this consumes the next queued delivery, even
    /// when its content is identical to another message in the run.
    fn emit_injected(&self, event: AgentEvent) -> futures::future::BoxFuture<'static, ()> {
        self.emit(event)
    }

    /// Sync non-blocking emit for the tool `on_update` path. Must never panic
    /// and must never block (it's called from a `&dyn Fn` closure inside
    /// `execute`). Default impl is a no-op so custom emitters opt in.
    fn try_emit(&self, _event: AgentEvent) {}
}

/// An emitter that fans out to `broadcast::Sender<AgentEvent>` subscribers.
pub struct BroadcastEmitter {
    tx: tokio::sync::broadcast::Sender<AgentEvent>,
}

impl BroadcastEmitter {
    pub fn new(buffer: usize) -> (Self, tokio::sync::broadcast::Receiver<AgentEvent>) {
        let (tx, rx) = tokio::sync::broadcast::channel(buffer);
        (Self { tx }, rx)
    }

    pub fn from_sender(tx: tokio::sync::broadcast::Sender<AgentEvent>) -> Self {
        Self { tx }
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }

    pub fn try_emit(&self, event: AgentEvent) {
        let _ = self.tx.send(event);
    }
}

impl AgentEmitter for BroadcastEmitter {
    fn emit(&self, event: AgentEvent) -> futures::future::BoxFuture<'static, ()> {
        let _ = self.tx.send(event);
        Box::pin(async {})
    }
    fn try_emit(&self, event: AgentEvent) {
        let _ = self.tx.send(event);
    }
}

/// An emitter that collects every event into a `Mutex<Vec<AgentEvent>>`. Used
/// by tests and by `run_agent_loop` callers that just want the sequence.
pub struct CollectorEmitter {
    events: std::sync::Arc<std::sync::Mutex<Vec<AgentEvent>>>,
}

impl CollectorEmitter {
    pub fn new() -> (Self, std::sync::Arc<std::sync::Mutex<Vec<AgentEvent>>>) {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Self {
                events: Arc::clone(&events),
            },
            events,
        )
    }
}

impl Default for CollectorEmitter {
    fn default() -> Self {
        let (s, _) = Self::new();
        s
    }
}

impl AgentEmitter for CollectorEmitter {
    fn emit(&self, event: AgentEvent) -> futures::future::BoxFuture<'static, ()> {
        self.events.lock().expect("events lock").push(event);
        Box::pin(async {})
    }
    fn try_emit(&self, event: AgentEvent) {
        self.events.lock().expect("events lock").push(event);
    }
}
