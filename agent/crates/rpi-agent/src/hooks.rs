//! Mirrors `packages/agent/src/types.ts::AgentLoopConfig` — the config bundle
//! the low-level loop consumes. TS `AgentLoopConfig extends SimpleStreamOptions`
//! and adds `model` + a set of async hooks. Rust models it as a struct of
//! `Option<Arc<dyn Fn>>>` hooks (required `convert_to_llm` is non-optional),
//! the `model`, and a `SimpleStreamOptions`-derived subset the loop forwards
//! to `StreamFn`.
//!
//! Hook signatures use `Arc<dyn Fn(...) -> BoxFuture<'static, T> + Send + Sync>`
//! so an `Agent` can store closures that capture `Arc<Agent>` / shared state
//! without lifetime gymnastics. Every hook mirrors the TS contract: must not
//! throw — return a safe fallback instead.

use crate::message::AgentMessage;
use crate::types::{
    AfterToolCallContext, AfterToolCallResult, AgentLoopTurnUpdate, BeforeToolCallContext,
    BeforeToolCallResult, ShouldStopAfterTurnContext, ToolExecutionMode,
};
use futures::future::BoxFuture;
use rpi_ai::provider::{CacheRetention, SimpleStreamOptions};
use rpi_ai::types::{Message, ThinkingLevel};
use rpi_ai::Model;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// `(messages: AgentMessage[]) -> Message[]` — the required
/// LLM-boundary converter. Filters/transforms custom messages into the
/// provider-facing `Message` union. Must not panic.
pub type ConvertToLlm =
    Arc<dyn Fn(Vec<AgentMessage>) -> BoxFuture<'static, Vec<Message>> + Send + Sync>;

/// `(messages, signal) -> AgentMessage[]` — optional pre-convert transform at
/// the `AgentMessage` level (context-window pruning, external injection).
pub type TransformContext = Arc<
    dyn Fn(Vec<AgentMessage>, CancellationToken) -> BoxFuture<'static, Vec<AgentMessage>>
        + Send
        + Sync,
>;

/// `(provider: &str) -> Option<String>` — dynamic API-key resolver per turn.
pub type GetApiKey = Arc<dyn Fn(&str) -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// `(context) -> bool` — return true to stop after the current turn (before
/// steering/follow-up drain).
pub type ShouldStopAfterTurn =
    Arc<dyn Fn(ShouldStopAfterTurnContext<'_>) -> BoxFuture<'static, bool> + Send + Sync>;

/// `(context) -> Option<AgentLoopTurnUpdate>` — replacement context/model/thinking
/// for the next turn, if any.
pub type PrepareNextTurn = Arc<
    dyn Fn(ShouldStopAfterTurnContext<'_>) -> BoxFuture<'static, Option<AgentLoopTurnUpdate>>
        + Send
        + Sync,
>;

/// Hook invoked after a tool-result batch is appended and before the next
/// assistant request. Implementations may replace the context (for example by
/// inserting a compaction boundary).
pub type AfterToolResults = Arc<
    dyn Fn(ShouldStopAfterTurnContext<'_>) -> BoxFuture<'static, Option<AgentLoopTurnUpdate>>
        + Send
        + Sync,
>;

/// `() -> Vec<AgentMessage>` — messages to inject mid-run after a tool batch.
pub type GetSteeringMessages = Arc<dyn Fn() -> BoxFuture<'static, Vec<AgentMessage>> + Send + Sync>;

/// `() -> usize` — how many messages are queued right now, **without draining**.
///
/// Sync (not a future) on purpose: it runs on the settle path of every turn
/// and only needs to read two `Mutex<VecDeque>` lengths.
pub type PendingMessageCount = Arc<dyn Fn() -> usize + Send + Sync>;

/// `() -> Vec<AgentMessage>` — messages to inject after the loop would stop.
pub type GetFollowUpMessages = Arc<dyn Fn() -> BoxFuture<'static, Vec<AgentMessage>> + Send + Sync>;

/// `(context, signal) -> Option<BeforeToolCallResult>` — can block a tool call
/// before it runs.
pub type BeforeToolCall = Arc<
    dyn Fn(
            BeforeToolCallContext<'_>,
            CancellationToken,
        ) -> BoxFuture<'static, Option<BeforeToolCallResult>>
        + Send
        + Sync,
>;

/// `(context, signal) -> Option<AfterToolCallResult>` — overrides fields of an
/// executed tool result before `tool_execution_end` / `MessageEnd`.
pub type AfterToolCall = Arc<
    dyn Fn(
            AfterToolCallContext<'_>,
            CancellationToken,
        ) -> BoxFuture<'static, Option<AfterToolCallResult>>
        + Send
        + Sync,
>;

/// The config bundle handed to `run_agent_loop` / `run_agent_loop_continue`.
///
/// An optional incremental converter for the common case where the context is
/// unchanged except for messages appended at its tail. It must preserve the
/// same projection as `convert_to_llm` for the supplied suffix.
pub type IncrementalConvertToLlm = Arc<dyn Fn(&[AgentMessage]) -> Vec<Message> + Send + Sync>;

/// Mirrors TS `AgentLoopConfig`. Hook fields are `Option` except `convert_to_llm`
/// (required). The provider-options subset (`api_key`, `timeout`, `cache_retention`,
/// `session_id`, `max_retries`, `max_retry_delay`, `signal`) is kept as plain
/// fields the loop stuffs into a `SimpleStreamOptions` per turn.
#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: Model,

    /// Required: `AgentMessage[]` → `Message[]`. Never `None` at call time; the
    /// `AgentBuilder` installs `default_convert_to_llm` (drop custom roles) when
    /// the caller doesn't supply one.
    pub convert_to_llm: ConvertToLlm,
    /// Optional fast path for appending provider messages when no context
    /// transform is installed. Custom converters can leave this unset and use
    /// the original full conversion path.
    pub convert_to_llm_incremental: Option<IncrementalConvertToLlm>,

    pub transform_context: Option<TransformContext>,
    pub get_api_key: Option<GetApiKey>,
    pub should_stop_after_turn: Option<ShouldStopAfterTurn>,
    pub prepare_next_turn: Option<PrepareNextTurn>,
    pub after_tool_results: Option<AfterToolResults>,
    pub get_steering_messages: Option<GetSteeringMessages>,
    pub get_follow_up_messages: Option<GetFollowUpMessages>,
    /// Read-only count of what is **still queued** (steering + follow-up).
    ///
    /// Needed because `get_steering_messages` / `get_follow_up_messages` are
    /// **draining** hooks — calling them to ask "how many are left?" would
    /// consume the queue. `TurnEnd` reports this count as `pending_messages`
    /// so a plugin can show an "N queued" indicator.
    pub pending_message_count: Option<PendingMessageCount>,
    pub before_tool_call: Option<BeforeToolCall>,
    pub after_tool_call: Option<AfterToolCall>,

    /// Per-batch execution mode. Default `Parallel`.
    pub tool_execution: ToolExecutionMode,

    // ---- provider-options subset (forwarded into SimpleStreamOptions) ----
    pub thinking_level: ThinkingLevel,
    pub api_key: Option<String>,
    pub timeout: Option<Duration>,
    /// Maximum silence between streamed response bytes; zero disables it.
    pub stream_idle_timeout: Option<Duration>,
    pub max_retries: Option<u32>,
    pub max_retry_delay: Option<Duration>,
    pub cache_retention: CacheRetention,
    pub session_id: Option<String>,
    /// Cancellation token for the whole run. The loop clones a child per tool.
    pub signal: CancellationToken,
}

impl std::fmt::Debug for AgentLoopConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLoopConfig")
            .field("model", &self.model)
            .field("tool_execution", &self.tool_execution)
            .field("thinking_level", &self.thinking_level)
            .field("cache_retention", &self.cache_retention)
            .field("session_id", &self.session_id)
            .field("transform_context", &self.transform_context.is_some())
            .field(
                "convert_to_llm_incremental",
                &self.convert_to_llm_incremental.is_some(),
            )
            .field("get_api_key", &self.get_api_key.is_some())
            .field(
                "should_stop_after_turn",
                &self.should_stop_after_turn.is_some(),
            )
            .field("prepare_next_turn", &self.prepare_next_turn.is_some())
            .field("after_tool_results", &self.after_tool_results.is_some())
            .field(
                "get_steering_messages",
                &self.get_steering_messages.is_some(),
            )
            .field(
                "get_follow_up_messages",
                &self.get_follow_up_messages.is_some(),
            )
            .field("before_tool_call", &self.before_tool_call.is_some())
            .field("after_tool_call", &self.after_tool_call.is_some())
            .finish()
    }
}

/// Build a `SimpleStreamOptions` from this config's provider-options subset,
/// overriding `signal` with the run's token. Mirrors the TS spread
/// `{ ...config, apiKey: resolvedApiKey, signal }` passed to `streamFunction`.
impl AgentLoopConfig {
    pub fn to_stream_options(&self, api_key: Option<String>) -> SimpleStreamOptions {
        let mut opts = SimpleStreamOptions {
            api_key,
            timeout: self.timeout,
            stream_idle_timeout: self.stream_idle_timeout,
            max_retries: self.max_retries,
            max_retry_delay: self.max_retry_delay,
            headers: None,
            metadata: None,
            cache_retention: self.cache_retention,
            session_id: self.session_id.clone(),
            signal: self.signal.clone(),
            ..SimpleStreamOptions::default()
        };
        // Forward the run's thinking level as `reasoning` (the provider-level
        // knob) so a real provider like anthropic can map it to adaptive vs
        // budget-based thinking. TS `AgentLoopConfig extends SimpleStreamOptions`
        // and so carries `reasoning` through directly.
        match self.thinking_level {
            ThinkingLevel::Off => opts.reasoning = None,
            other => opts.reasoning = Some(other),
        }
        opts
    }
}

/// The default `convert_to_llm`: keep only `user`/`assistant`/`toolResult`
/// messages, drop `custom`. Mirrors TS `defaultConvertToLlm`.
pub fn default_convert_to_llm(messages: Vec<AgentMessage>) -> Vec<Message> {
    let mut converted = Vec::with_capacity(messages.len());
    for message in messages {
        match message {
            AgentMessage::User(user) => converted.push(Message::User(user)),
            AgentMessage::Assistant(assistant) => converted.push(Message::Assistant(assistant)),
            AgentMessage::ToolResult(tool_result) => {
                converted.push(Message::ToolResult(tool_result))
            }
            AgentMessage::Custom(_) => {}
        }
    }
    converted
}

pub fn default_convert_to_llm_incremental(messages: &[AgentMessage]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::User(user) => Some(Message::User(user.clone())),
            AgentMessage::Assistant(assistant) => Some(Message::Assistant(assistant.clone())),
            AgentMessage::ToolResult(tool_result) => Some(Message::ToolResult(tool_result.clone())),
            AgentMessage::Custom(_) => None,
        })
        .collect()
}

pub fn default_convert_to_llm_incremental_fn() -> IncrementalConvertToLlm {
    Arc::new(default_convert_to_llm_incremental)
}

/// Wrap a `default_convert_to_llm` into the `ConvertToLlm` Arc shape expected
/// by `AgentLoopConfig`.
pub fn default_convert_to_llm_fn() -> ConvertToLlm {
    Arc::new(|messages: Vec<AgentMessage>| {
        let out = default_convert_to_llm(messages);
        Box::pin(async move { out })
    })
}
