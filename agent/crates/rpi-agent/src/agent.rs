//! Mirrors `packages/agent/src/agent.ts` — the stateful wrapper around the
//! low-level agent loop. `Agent` owns the current transcript, emits lifecycle
//! events to subscribers, executes tools, and exposes queueing APIs for
//! steering and follow-up messages.
//!
//! The TS `Agent` class drives the loop via `runWithLifecycle` +
//! `processEvents`. Rust models the same shape:
//! - internal `Mutex<MutableAgentState>` for the transcript + runtime flags;
//! - a `broadcast::Sender<AgentEvent>` so multiple subscribers each get their
//!   own copy;
//! - a state-reducing emitter ([`StatefulEmitter`]) that folds each event into
//!   `MutableAgentState` FIRST, then broadcasts — the direct mirror of TS
//!   `processEvents` ("reduce internal state, then await listeners");
//! - a per-run `ActiveRun` (abort handle + completion notify) so `prompt`
//!   blocks until the run settles and `abort`/`wait_for_idle` can act on it.

use crate::agent_loop::{run_agent_loop, run_agent_loop_continue};
use crate::events::{AgentEmitter, AgentEvent, BroadcastEmitter};
use crate::hooks::{
    default_convert_to_llm_fn, default_convert_to_llm_incremental_fn, AgentLoopConfig, ConvertToLlm,
};
use crate::message::AgentMessage;
use crate::queue::PendingMessageQueue;
use crate::stream_fn::{get_default_stream_fn, StreamFn};
use crate::types::{AgentContext, AgentState, QueueMode, ToolExecutionMode};

use rpi_ai::types::{AssistantMessage, UserContent, UserMessage};
use rpi_ai::Model;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, Notify};
use tokio_util::sync::CancellationToken;

/// Options for constructing an [`Agent`]. Mirrors TS `AgentOptions`. Only
/// `stream_fn` is required (or a default must have been installed process-wide
/// via [`crate::stream_fn::set_default_stream_fn`]).
#[derive(Default)]
pub struct AgentOptions {
    pub initial_state: Option<InitialState>,
    pub convert_to_llm: Option<ConvertToLlm>,
    pub stream_fn: Option<StreamFn>,
    pub thinking_level: Option<rpi_ai::types::ThinkingLevel>,
    pub queue_mode: Option<QueueMode>,
    pub follow_up_mode: Option<QueueMode>,
    pub tool_execution: Option<ToolExecutionMode>,
    pub session_id: Option<String>,
}

/// Subset of `AgentState` settable at construction.
#[derive(Default)]
pub struct InitialState {
    pub system_prompt: Option<String>,
    pub model: Option<Model>,
    pub thinking_level: Option<rpi_ai::types::ThinkingLevel>,
    pub tools: Option<Vec<Arc<dyn crate::agent_tool::AgentTool>>>,
    pub messages: Option<Vec<AgentMessage>>,
}

/// The owned mutable state. Mirrors TS `MutableAgentState`. Guarded by a mutex.
struct MutableAgentState {
    system_prompt: String,
    model: Model,
    thinking_level: rpi_ai::types::ThinkingLevel,
    tools: Vec<Arc<dyn crate::agent_tool::AgentTool>>,
    messages: Vec<AgentMessage>,
    is_streaming: bool,
    /// The in-flight assistant message. `Arc` so the per-delta `MessageUpdate`
    /// does not deep-clone it into this field on every streaming chunk.
    streaming_message: Option<Arc<AssistantMessage>>,
    pending_tool_calls: HashSet<String>,
    error_message: Option<String>,
}

impl MutableAgentState {
    fn snapshot(&self) -> AgentState {
        AgentState {
            system_prompt: self.system_prompt.clone(),
            model: self.model.clone(),
            thinking_level: self.thinking_level,
            tools: self.tools.clone(),
            messages: self.messages.clone(),
            is_streaming: self.is_streaming,
            streaming_message: self.streaming_message.clone(),
            pending_tool_calls: self.pending_tool_calls.clone(),
            error_message: self.error_message.clone(),
        }
    }

    /// Reduce an event into state. Mirrors the switch in TS `processEvents`.
    fn reduce(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::MessageStart { message, .. } => {
                // One clone per message (not per delta): the state holds the
                // partial as a shared snapshot from here on.
                self.streaming_message = message
                    .as_assistant()
                    .map(|assistant| Arc::new(assistant.clone()));
            }
            AgentEvent::MessageUpdate { message, .. } => {
                self.streaming_message = Some(Arc::clone(message));
            }
            AgentEvent::MessageEnd { message, .. } => {
                self.streaming_message = None;
                self.messages.push(message.clone());
            }
            AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                self.pending_tool_calls.insert(tool_call_id.clone());
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                self.pending_tool_calls.remove(tool_call_id);
            }
            AgentEvent::TurnEnd { message, .. } => {
                if let Some(am) = message.as_assistant() {
                    if am.error_message.is_some() {
                        self.error_message = am.error_message.clone();
                    }
                }
            }
            AgentEvent::AgentEnd { .. } => {
                self.streaming_message = None;
            }
            _ => {}
        }
    }
}

/// Per-run handle. Replaces TS `ActiveRun`.
struct ActiveRun {
    abort: CancellationToken,
    done: Arc<Notify>,
}

/// A state-reducing emitter: folds each event into `MutableAgentState` FIRST,
/// then broadcasts to subscribers. The direct mirror of TS `processEvents`.
struct StatefulEmitter {
    state: Arc<Mutex<MutableAgentState>>,
    broadcast: BroadcastEmitter,
}

impl AgentEmitter for StatefulEmitter {
    fn emit(&self, event: AgentEvent) -> futures::future::BoxFuture<'static, ()> {
        self.state.lock().expect("state lock").reduce(&event);
        self.broadcast.try_emit(event);
        Box::pin(async {})
    }
    fn try_emit(&self, event: AgentEvent) {
        self.state.lock().expect("state lock").reduce(&event);
        self.broadcast.try_emit(event);
    }
}

/// Shared queue storage: `Arc<Mutex<PendingMessageQueue>>` so the
/// `get_steering_messages` / `get_follow_up_messages` hook closures (which must
/// be `'static + Send + Sync`) can capture a clone.
type SharedQueue = Arc<Mutex<PendingMessageQueue>>;

/// A stateful agent. Clone shares the same inner state + event channel (like
/// holding a second reference to the TS `Agent` instance).
#[derive(Clone)]
pub struct Agent {
    inner: Arc<Inner>,
}

struct Inner {
    state: Arc<Mutex<MutableAgentState>>,
    convert_to_llm: ConvertToLlm,
    convert_to_llm_incremental: Option<crate::hooks::IncrementalConvertToLlm>,
    stream_fn: StreamFn,
    steering_queue: SharedQueue,
    follow_up_queue: SharedQueue,
    session_id: Option<String>,
    tool_execution: ToolExecutionMode,
    event_tx: broadcast::Sender<AgentEvent>,
    active_run: Mutex<Option<ActiveRun>>,
}

/// Builder for [`Agent`] with a fluent API. Mirrors the TS `new Agent(opts)`.
pub struct AgentBuilder {
    opts: AgentOptions,
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self {
            opts: AgentOptions::default(),
        }
    }

    pub fn model(mut self, model: Model) -> Self {
        self.opts
            .initial_state
            .get_or_insert_with(InitialState::default)
            .model = Some(model);
        self
    }

    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.opts
            .initial_state
            .get_or_insert_with(InitialState::default)
            .system_prompt = Some(prompt.into());
        self
    }

    pub fn thinking_level(mut self, level: rpi_ai::types::ThinkingLevel) -> Self {
        self.opts
            .initial_state
            .get_or_insert_with(InitialState::default)
            .thinking_level = Some(level);
        self
    }

    pub fn tools(mut self, tools: Vec<Arc<dyn crate::agent_tool::AgentTool>>) -> Self {
        self.opts
            .initial_state
            .get_or_insert_with(InitialState::default)
            .tools = Some(tools);
        self
    }

    pub fn messages(mut self, messages: Vec<AgentMessage>) -> Self {
        self.opts
            .initial_state
            .get_or_insert_with(InitialState::default)
            .messages = Some(messages);
        self
    }

    pub fn stream_fn(mut self, stream_fn: StreamFn) -> Self {
        self.opts.stream_fn = Some(stream_fn);
        self
    }

    pub fn convert_to_llm(mut self, f: ConvertToLlm) -> Self {
        self.opts.convert_to_llm = Some(f);
        self
    }

    pub fn queue_mode(mut self, mode: QueueMode) -> Self {
        self.opts.queue_mode = Some(mode);
        self
    }

    pub fn follow_up_mode(mut self, mode: QueueMode) -> Self {
        self.opts.follow_up_mode = Some(mode);
        self
    }

    pub fn tool_execution(mut self, mode: ToolExecutionMode) -> Self {
        self.opts.tool_execution = Some(mode);
        self
    }

    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.opts.session_id = Some(id.into());
        self
    }

    /// Build the `Agent`. Errors if no `stream_fn` was supplied and no default
    /// has been installed process-wide.
    pub fn build(self) -> Result<Agent, crate::AgentError> {
        let stream_fn = match self.opts.stream_fn {
            Some(f) => f,
            None => get_default_stream_fn()?,
        };

        let initial = self.opts.initial_state.unwrap_or_default();
        let model = initial.model.unwrap_or_else(default_model);
        let thinking_level = self
            .opts
            .thinking_level
            .or(initial.thinking_level)
            .unwrap_or(rpi_ai::types::ThinkingLevel::Off);
        let default_converter = self.opts.convert_to_llm.is_none();
        let convert_to_llm = self
            .opts
            .convert_to_llm
            .unwrap_or_else(default_convert_to_llm_fn);
        let convert_to_llm_incremental = if default_converter {
            Some(default_convert_to_llm_incremental_fn())
        } else {
            None
        };

        let state = MutableAgentState {
            system_prompt: initial.system_prompt.unwrap_or_default(),
            model: model.clone(),
            thinking_level,
            tools: initial.tools.unwrap_or_default(),
            messages: initial.messages.unwrap_or_default(),
            is_streaming: false,
            streaming_message: None,
            pending_tool_calls: HashSet::new(),
            error_message: None,
        };

        let (event_tx, _) = broadcast::channel(256);

        let queue_mode = self.opts.queue_mode.unwrap_or_default();
        let follow_up_mode = self.opts.follow_up_mode.unwrap_or_default();

        let inner = Inner {
            state: Arc::new(Mutex::new(state)),
            convert_to_llm,
            convert_to_llm_incremental,
            stream_fn,
            steering_queue: Arc::new(Mutex::new(PendingMessageQueue::new(queue_mode))),
            follow_up_queue: Arc::new(Mutex::new(PendingMessageQueue::new(follow_up_mode))),
            session_id: self.opts.session_id,
            tool_execution: self.opts.tool_execution.unwrap_or_default(),
            event_tx,
            active_run: Mutex::new(None),
        };

        Ok(Agent {
            inner: Arc::new(inner),
        })
    }
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent {
    /// Subscribe to agent lifecycle events. Returns a `broadcast::Receiver`.
    /// Mirrors TS `subscribe`.
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.inner.event_tx.subscribe()
    }

    /// Current agent state snapshot. Clones tools/messages so callers can't
    /// mutate internal state. Mirrors TS `get state()`.
    pub fn state(&self) -> AgentState {
        self.inner.state.lock().expect("state lock").snapshot()
    }

    /// Queue a steering message (injected after the current turn's tool batch).
    pub fn steer(&self, message: AgentMessage) {
        self.inner
            .steering_queue
            .lock()
            .expect("steer lock")
            .enqueue(message);
    }

    /// Queue a follow-up message (injected when the agent would otherwise stop).
    pub fn follow_up(&self, message: AgentMessage) {
        self.inner
            .follow_up_queue
            .lock()
            .expect("followup lock")
            .enqueue(message);
    }

    /// True when either queue has pending messages.
    pub fn has_queued_messages(&self) -> bool {
        let s = self.inner.steering_queue.lock().expect("steer lock");
        if !s.is_empty() {
            return true;
        }
        let f = self.inner.follow_up_queue.lock().expect("followup lock");
        !f.is_empty()
    }

    /// Abort the current run, if one is active. No-op otherwise.
    pub fn abort(&self) {
        if let Some(run) = self.inner.active_run.lock().expect("run lock").as_ref() {
            run.abort.cancel();
        }
    }

    /// Resolve when the current run finishes (or immediately if idle).
    pub async fn wait_for_idle(&self) {
        let notify = {
            let guard = self.inner.active_run.lock().expect("run lock");
            guard.as_ref().map(|r| Arc::clone(&r.done))
        };
        if let Some(n) = notify {
            n.notified().await;
        }
    }

    /// Start a new prompt from text. Convenience for `prompt_message`.
    pub async fn prompt(&self, text: impl Into<String>) -> Result<(), crate::AgentError> {
        let message = AgentMessage::User(UserMessage::new(
            UserContent::Text(text.into()),
            crate::clock::now_ms(),
        ));
        self.prompt_messages(vec![message]).await
    }

    /// Start a new prompt from a single `AgentMessage`.
    pub async fn prompt_message(&self, message: AgentMessage) -> Result<(), crate::AgentError> {
        self.prompt_messages(vec![message]).await
    }

    /// Start a new prompt from a batch of `AgentMessage`s.
    pub async fn prompt_messages(
        &self,
        messages: Vec<AgentMessage>,
    ) -> Result<(), crate::AgentError> {
        self.start_active_run()?;
        let run_done = self.current_done();
        let outcome = self.run_prompt(messages).await;
        self.finish_run();
        if let Some(n) = run_done {
            n.notify_waiters();
        }
        outcome
    }

    /// Continue from the current transcript. Errors if the agent is busy or
    /// the last message is an assistant message with no queued steering/follow-up.
    pub async fn continue_run(&self) -> Result<(), crate::AgentError> {
        self.start_active_run()?;
        let run_done = self.current_done();
        let outcome = self.run_continue().await;
        self.finish_run();
        if let Some(n) = run_done {
            n.notify_waiters();
        }
        outcome
    }

    /// Clear transcript + runtime state + queues. Errors if a run is active.
    pub fn reset(&self) -> Result<(), crate::AgentError> {
        let mut state = self.inner.state.lock().expect("state lock");
        if self.inner.active_run.lock().expect("run lock").is_some() {
            return Err(crate::AgentError::State(
                "Agent is already processing. Wait for completion before resetting.".into(),
            ));
        }
        state.messages.clear();
        state.is_streaming = false;
        state.streaming_message = None;
        state.pending_tool_calls.clear();
        state.error_message = None;
        drop(state);
        let _ = self
            .inner
            .steering_queue
            .lock()
            .expect("steer lock")
            .try_drain();
        let _ = self
            .inner
            .follow_up_queue
            .lock()
            .expect("followup lock")
            .try_drain();
        Ok(())
    }

    // ---- internals --------------------------------------------------------

    fn start_active_run(&self) -> Result<(), crate::AgentError> {
        let mut guard = self.inner.active_run.lock().expect("run lock");
        if guard.is_some() {
            return Err(crate::AgentError::State(
                "Agent is already processing a prompt. Use steer() or followUp() to queue messages, or wait for completion.".into(),
            ));
        }
        let abort = CancellationToken::new();
        let done = Arc::new(Notify::new());
        *guard = Some(ActiveRun {
            abort: abort.clone(),
            done: Arc::clone(&done),
        });

        let mut state = self.inner.state.lock().expect("state lock");
        state.is_streaming = true;
        state.streaming_message = None;
        state.error_message = None;
        drop(state);
        Ok(())
    }

    fn finish_run(&self) {
        {
            let mut state = self.inner.state.lock().expect("state lock");
            state.is_streaming = false;
            state.streaming_message = None;
            state.pending_tool_calls.clear();
        }
        let mut guard = self.inner.active_run.lock().expect("run lock");
        *guard = None;
    }

    fn current_done(&self) -> Option<Arc<Notify>> {
        self.inner
            .active_run
            .lock()
            .expect("run lock")
            .as_ref()
            .map(|r| Arc::clone(&r.done))
    }

    fn abort_token(&self) -> CancellationToken {
        self.inner
            .active_run
            .lock()
            .expect("run lock")
            .as_ref()
            .map(|r| r.abort.clone())
            .unwrap_or_else(CancellationToken::new)
    }

    fn context_snapshot(&self) -> AgentContext {
        let state = self.inner.state.lock().expect("state lock");
        AgentContext {
            system_prompt: state.system_prompt.clone(),
            messages: state.messages.clone(),
            tools: state.tools.clone(),
        }
    }

    fn build_config(&self, signal: CancellationToken) -> AgentLoopConfig {
        let state = self.inner.state.lock().expect("state lock");
        let steering = Arc::clone(&self.inner.steering_queue);
        let follow_up = Arc::clone(&self.inner.follow_up_queue);
        // 两个队列的**只读**引用，给 `pending_message_count` 用（它必须与
        // 上面那两个 drain hook 读同一份数据，否则“显示 N 条”与“真的还在
        // 排队 N 条”会脱节）。
        let steering_count = Arc::clone(&self.inner.steering_queue);
        let follow_count = Arc::clone(&self.inner.follow_up_queue);
        AgentLoopConfig {
            model: state.model.clone(),
            convert_to_llm: Arc::clone(&self.inner.convert_to_llm),
            convert_to_llm_incremental: self.inner.convert_to_llm_incremental.clone(),
            transform_context: None,
            get_api_key: None,
            should_stop_after_turn: None,
            prepare_next_turn: None,
            after_tool_results: None,
            get_steering_messages: Some(Arc::new(move || {
                let q = Arc::clone(&steering);
                Box::pin(async move { q.lock().expect("steer lock").try_drain() })
            })),
            get_follow_up_messages: Some(Arc::new(move || {
                let q = Arc::clone(&follow_up);
                Box::pin(async move { q.lock().expect("followup lock").try_drain() })
            })),
            // 只读计数，**不消耗队列**（两个 drain hook 都会把消息取走，
            // 拿它们来“问还剩几条”等于把排队的消息吃掉）。
            pending_message_count: Some(Arc::new(move || {
                let steer = steering_count.lock().expect("steer lock").len();
                let follow = follow_count.lock().expect("followup lock").len();
                steer + follow
            })),
            before_tool_call: None,
            after_tool_call: None,
            tool_execution: self.inner.tool_execution,
            thinking_level: state.thinking_level,
            api_key: None,
            timeout: None,
            stream_idle_timeout: None,
            max_retries: None,
            max_retry_delay: None,
            cache_retention: rpi_ai::provider::CacheRetention::default(),
            session_id: self.inner.session_id.clone(),
            signal,
        }
    }

    async fn run_prompt(&self, messages: Vec<AgentMessage>) -> Result<(), crate::AgentError> {
        let signal = self.abort_token();
        let context = self.context_snapshot();
        let config = self.build_config(signal);
        let emit: Arc<dyn AgentEmitter> = Arc::new(StatefulEmitter {
            state: Arc::clone(&self.inner.state),
            broadcast: BroadcastEmitter::from_sender(self.inner.event_tx.clone()),
        });
        let stream_fn = Arc::clone(&self.inner.stream_fn);
        run_agent_loop(messages, context, config, emit, stream_fn).await?;
        Ok(())
    }

    async fn run_continue(&self) -> Result<(), crate::AgentError> {
        let signal = self.abort_token();
        let context = self.context_snapshot();
        if context.messages.is_empty() {
            return Err(crate::AgentError::State(
                "No messages to continue from".into(),
            ));
        }
        if context.messages.last().unwrap().is_assistant() {
            return Err(crate::AgentError::State(
                "Cannot continue from message role: assistant".into(),
            ));
        }
        let config = self.build_config(signal);
        let emit: Arc<dyn AgentEmitter> = Arc::new(StatefulEmitter {
            state: Arc::clone(&self.inner.state),
            broadcast: BroadcastEmitter::from_sender(self.inner.event_tx.clone()),
        });
        let stream_fn = Arc::clone(&self.inner.stream_fn);
        run_agent_loop_continue(context, config, emit, stream_fn).await?;
        Ok(())
    }
}

fn default_model() -> Model {
    rpi_ai::model::Model::new(
        "unknown",
        "unknown",
        rpi_ai::types::Api::Other("unknown".into()),
        "unknown",
        "",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ai::event_stream::create_assistant_message_event_stream;
    use rpi_ai::provider::Provider;
    use rpi_ai::providers::faux::{FauxProvider, FauxScript};

    fn faux_stream_fn(provider: Arc<FauxProvider>) -> StreamFn {
        crate::stream_fn::stream_fn(move |model, ctx, opts| {
            // The faux provider's stream_simple is async; StreamFn is sync-return.
            // Bridge by spawning the producer ourselves.
            let (mut prod, stream) = create_assistant_message_event_stream();
            let p = Arc::clone(&provider);
            let model = model.clone();
            let ctx = ctx.clone();
            let opts = opts.clone();
            tokio::spawn(async move {
                let mut s = p.stream_simple(&model, &ctx, &opts).await;
                // Drain the real stream into our producer.
                while let Some(ev) = s.next().await {
                    if !prod.push(ev).await {
                        break;
                    }
                }
            });
            stream
        })
    }

    #[tokio::test]
    async fn builder_requires_stream_fn() {
        let res = AgentBuilder::new().build();
        assert!(res.is_err(), "build with no stream_fn should error");
    }

    #[tokio::test]
    async fn prompt_with_faux_text_collects_events() {
        let provider = FauxProvider::new(FauxScript::new().with_text("hello"));
        let sf = faux_stream_fn(provider);
        let agent = AgentBuilder::new().stream_fn(sf).build().unwrap();

        let mut rx = agent.subscribe();
        agent.prompt("hi").await.unwrap();

        // Drain the broadcast until AgentEnd.
        let mut saw_start = false;
        let mut saw_end = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AgentEvent::AgentStart => saw_start = true,
                AgentEvent::AgentEnd { .. } => saw_end = true,
                _ => {}
            }
        }
        assert!(saw_start, "agent_start observed");
        assert!(saw_end, "agent_end observed");
        // State: 1 user prompt + 1 assistant reply.
        assert_eq!(agent.state().messages.len(), 2);
    }
}
