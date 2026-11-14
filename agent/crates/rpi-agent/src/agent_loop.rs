//! Mirrors `packages/agent/src/agent-loop.ts` — the provider-agnostic agent loop.
//!
//! Two public free functions, [`run_agent_loop`] (new prompt) and
//! [`run_agent_loop_continue`] (no new prompt), drive the outer follow-up loop
//! and inner steering+tool loop. The only LLM boundary is [`StreamFn`]
//! (sync return → `AssistantMessageEventStream`); everything else talks in
//! [`AgentMessage`].
//!
//! Critical invariants enforced here (plan §5):
//! - **Tool-execution ordering**: in a parallel batch, `ToolExecutionEnd` fires
//!   in *completion* order; tool-result `MessageStart`/`MessageEnd` fire later
//!   in *source/ordinal* order. Implemented by collecting completion signals
//!   into a queue, then walking the finalized vec by index for the result
//!   messages.
//! - **Truncate-fail**: `stop_reason == Length` → every tool call in the
//!   message fails-in-place with `is_error:true` and is *not* executed.
//! - **Late-update suppression**: `on_update` after `execute` resolves is a
//!   no-op via an `accepting_updates: Arc<AtomicBool>` flipped false on settle.
//!
//! The loop is fully testable without [`crate::Agent`] — `run_agent_loop` takes
//! plain `AgentContext`/`AgentLoopConfig` + an `AgentEmitter`.

use crate::agent_tool::AgentTool;
use crate::error::AgentError;
use crate::events::{AgentEmitter, AgentEvent};
use crate::hooks::AgentLoopConfig;
use crate::message::AgentMessage;
use crate::stream_fn::StreamFn;
use crate::types::{
    AfterToolCallContext, AgentContext, AgentToolResult, BeforeToolCallContext, TextContentOrImage,
    ToolExecutionMode,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt, StreamExt};
use rpi_ai::types::{
    AssistantMessage, AssistantMessageEvent, Content, Message, StopReason, ToolCall,
    ToolResultMessage, ToolResultRole,
};
use rpi_ai::validate_tool_arguments;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

/// The messages a single `run_agent_loop` invocation added (prompt + assistant
/// + tool results + injected steering/follow-ups). Returned to the caller.
pub type NewMessages = Vec<AgentMessage>;

static AGENT_DIAGNOSTICS: OnceLock<bool> = OnceLock::new();

fn agent_diagnostics_enabled() -> bool {
    *AGENT_DIAGNOSTICS
        .get_or_init(|| std::env::var_os("RPI_AGENT_DIAGNOSTICS").is_some_and(|value| value != "0"))
}

/// Why a run ended. `Completed` is the normal exit; `Aborted`/`Failed` are set
/// when the terminal assistant message carried `Aborted`/`Error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopOutcome {
    Completed,
    Aborted,
    Failed,
}

impl LoopOutcome {
    fn from_stop(stop: StopReason) -> Self {
        match stop {
            StopReason::Aborted => LoopOutcome::Aborted,
            StopReason::Error => LoopOutcome::Failed,
            _ => LoopOutcome::Completed,
        }
    }
}

// ----------------------------------------------------------------------------
// Public entry points
// ----------------------------------------------------------------------------

/// Run an agent loop starting from a new prompt. Mirrors TS `runAgentLoop`.
///
/// Emits `agent_start`, `turn_start`, then `message_start`/`message_end` for
/// each prompt, then drives [`run_loop`]. Returns the messages produced.
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    emit: Arc<dyn AgentEmitter>,
    stream_fn: StreamFn,
) -> Result<NewMessages, AgentError> {
    let mut new_messages: Vec<AgentMessage> = prompts.clone();
    // The caller transferred ownership. Only prompts need a separate copy for
    // the returned additions; the existing history can move into the loop.
    let mut current_context = context;
    current_context.messages.extend(prompts);

    emit_event(&emit, AgentEvent::AgentStart).await;
    emit_event(&emit, AgentEvent::TurnStart).await;
    for prompt in &new_messages {
        emit_event(
            &emit,
            AgentEvent::MessageStart {
                message_id: None,
                message: prompt.clone(),
            },
        )
        .await;
        emit_event(
            &emit,
            AgentEvent::MessageEnd {
                message_id: None,
                message: prompt.clone(),
            },
        )
        .await;
    }

    run_loop(
        &mut current_context,
        &mut new_messages,
        &config,
        &emit,
        &stream_fn,
        None,
    )
    .await?;
    Ok(new_messages)
}

/// Continue an agent loop from the existing context (no new prompt). Mirrors
/// TS `runAgentLoopContinue`. Errors if the context is empty or its last
/// message is an assistant message (the provider would reject that).
pub async fn run_agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    emit: Arc<dyn AgentEmitter>,
    stream_fn: StreamFn,
) -> Result<NewMessages, AgentError> {
    if context.messages.is_empty() {
        return Err(AgentError::State(
            "cannot continue: no messages in context".into(),
        ));
    }
    if context.messages.last().unwrap().is_assistant() {
        return Err(AgentError::State(
            "cannot continue from message role: assistant".into(),
        ));
    }

    let mut new_messages: Vec<AgentMessage> = Vec::new();
    let mut current_context = context;

    emit_event(&emit, AgentEvent::AgentStart).await;
    emit_event(&emit, AgentEvent::TurnStart).await;

    run_loop(
        &mut current_context,
        &mut new_messages,
        &config,
        &emit,
        &stream_fn,
        None,
    )
    .await?;
    Ok(new_messages)
}

/// Continue a run whose last assistant message has **not had its tools run**.
///
/// This is the case a *deferred* (long-poll) provider creates: the turn's
/// assistant message arrives from a poll rather than from the loop, so it is
/// already in the transcript while its tool calls are still unanswered. The next
/// provider request would be rejected (every provider refuses an assistant message
/// whose tool calls have no results), so the tools must run first — which is
/// exactly what this entry point does before continuing the loop normally.
///
/// Errors when the last message is not an assistant message: there would be
/// nothing to resume, and silently calling the provider instead would be wrong.
pub async fn run_agent_loop_from_assistant(
    context: AgentContext,
    config: AgentLoopConfig,
    emit: Arc<dyn AgentEmitter>,
    stream_fn: StreamFn,
) -> Result<NewMessages, AgentError> {
    let mut current_context = context;
    let Some(AgentMessage::Assistant(assistant)) = current_context.messages.last().cloned() else {
        return Err(AgentError::State(
            "cannot continue from an assistant whose tools already ran: the last message is not an              assistant message"
                .into(),
        ));
    };
    let mut new_messages: Vec<AgentMessage> = Vec::new();

    emit_event(&emit, AgentEvent::AgentStart).await;
    // The turn whose tools are about to run is the current turn.
    emit_event(&emit, AgentEvent::TurnStart).await;

    run_loop(
        &mut current_context,
        &mut new_messages,
        &config,
        &emit,
        &stream_fn,
        Some(*assistant),
    )
    .await?;
    Ok(new_messages)
}

// ----------------------------------------------------------------------------
// Main loop — mirrors TS runLoop
// ----------------------------------------------------------------------------

/// A finalized tool call: the raw call, the merged result, and the error flag.
#[derive(Clone)]
pub(crate) struct FinalizedToolCall {
    pub(crate) tool_call: ToolCall,
    pub(crate) result: AgentToolResult,
    pub(crate) is_error: bool,
}

/// A batch of executed tool calls: the per-call `ToolResultMessage`s (in source
/// order) and the early-terminate hint.
struct ExecutedToolBatch {
    messages: Vec<ToolResultMessage>,
    terminate: bool,
}

async fn run_loop(
    current_context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    config: &AgentLoopConfig,
    emit: &Arc<dyn AgentEmitter>,
    stream_fn: &StreamFn,
    // `pending_assistant`: an assistant message already sitting at the end of
    // `current_context` whose tool calls have not run yet (see
    // `run_agent_loop_from_assistant`). When set, the first iteration executes its
    // tools instead of calling the provider — and does *not* push it into
    // `new_messages`, because it is already in the transcript.
    pending_assistant: Option<AssistantMessage>,
) -> Result<LoopOutcome, AgentError> {
    let mut pending_assistant = pending_assistant;
    let mut provider_cache = ProviderMessageCache::new();
    let mut first_turn = true;
    // Check for steering messages at start (user may have typed while waiting).
    let mut pending_messages = drain_steering(config).await;

    // Outer loop: continues when queued follow-up messages arrive after the
    // agent would otherwise stop.
    loop {
        let mut has_more_tool_calls = true;

        // Inner loop: process tool calls and steering messages.
        while has_more_tool_calls || !pending_messages.is_empty() {
            if !first_turn {
                emit_event(emit, AgentEvent::TurnStart).await;
            } else {
                first_turn = false;
            }

            // Inject pending (steering/follow-up) messages before the next LLM call.
            if !pending_messages.is_empty() {
                // Steering/follow-up hooks may return any AgentMessage, not
                // only append-safe user messages. In particular, an injected
                // ToolResult can be orphaned; the full converter is responsible
                // for repairing/filtering that history. Do not feed such a
                // suffix through the incremental converter.
                provider_cache.invalidate();
                for message in pending_messages.drain(..) {
                    emit.emit_injected(AgentEvent::MessageStart {
                        message_id: None,
                        message: message.clone(),
                    })
                    .await;
                    emit_event(
                        emit,
                        AgentEvent::MessageEnd {
                            message_id: None,
                            message: message.clone(),
                        },
                    )
                    .await;
                    current_context.messages.push(message.clone());
                    new_messages.push(message);
                }
            }

            // Stream the assistant response — unless the caller handed us one
            // whose tools still have to run.
            let message = match pending_assistant.take() {
                Some(message) => message,
                None => {
                    let message = stream_assistant_response(
                        current_context,
                        config,
                        &mut provider_cache,
                        emit,
                        stream_fn,
                    )
                    .await?;
                    new_messages.push(AgentMessage::Assistant(Box::new(message.clone())));
                    message
                }
            };

            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                let am = AgentMessage::Assistant(Box::new(message.clone()));
                emit_event(
                    emit,
                    AgentEvent::TurnEnd {
                        message: am,
                        tool_results: Vec::new(),
                        pending_messages: pending_count(config),
                    },
                )
                .await;
                emit_event(
                    emit,
                    AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    },
                )
                .await;
                return Ok(LoopOutcome::from_stop(message.stop_reason));
            }

            // Collect tool calls (in content order = source/ordinal order).
            let tool_calls: Vec<ToolCall> = message
                .content
                .iter()
                .filter_map(|c| match c {
                    Content::ToolCall(tc) => Some(tc.clone()),
                    _ => None,
                })
                .collect();

            let mut tool_results: Vec<ToolResultMessage> = Vec::new();
            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                let batch = if matches!(message.stop_reason, StopReason::Length) {
                    // Truncate-fail invariant: Length → fail ALL without executing.
                    fail_tool_calls_from_truncated_message(&tool_calls, emit).await?
                } else {
                    execute_tool_calls(current_context, &message, &tool_calls, config, emit).await?
                };
                tool_results.extend(batch.messages);
                has_more_tool_calls = !batch.terminate;

                for result in &tool_results {
                    let am = AgentMessage::ToolResult(Box::new(result.clone()));
                    current_context.messages.push(am.clone());
                    new_messages.push(am);
                }
            }

            if !tool_results.is_empty() {
                if let Some(upd) = after_tool_results(
                    config,
                    &message,
                    &tool_results,
                    current_context,
                    new_messages,
                )
                .await
                {
                    if let Some(ctx) = upd.context {
                        *current_context = ctx;
                        provider_cache.invalidate();
                    }
                }
            }

            let am = AgentMessage::Assistant(Box::new(message.clone()));
            emit_event(
                emit,
                AgentEvent::TurnEnd {
                    message: am,
                    tool_results: tool_results.clone(),
                    pending_messages: pending_count(config),
                },
            )
            .await;

            // prepareNextTurn: replace context if provided. (Model/thinking swaps
            // are owned by Agent; run_loop borrows config immutably for hook
            // stability. M2 tests exercise context replacement only.)
            if let Some(upd) = prepare_next_turn(
                config,
                &message,
                &tool_results,
                current_context,
                new_messages,
            )
            .await
            {
                if let Some(ctx) = upd.context {
                    *current_context = ctx;
                    provider_cache.invalidate();
                }
            }

            if should_stop_after_turn(
                config,
                &message,
                &tool_results,
                current_context,
                new_messages,
            )
            .await
                && !(config.signal.is_cancelled() && !tool_calls.is_empty())
            {
                emit_event(
                    emit,
                    AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    },
                )
                .await;
                return Ok(LoopOutcome::Completed);
            }

            // Retain steering queued during the tool batch even on abort. A
            // cancelled tool continuation must end with a NEW terminal assistant:
            // public callers return messages, not LoopOutcome, and the harness
            // derives/persists its authoritative outcome from that assistant.
            pending_messages = drain_steering(config).await;
            if config.signal.is_cancelled() && !tool_calls.is_empty() {
                emit_event(emit, AgentEvent::TurnStart).await;
                let aborted = AgentMessage::Assistant(Box::new(AssistantMessage::terminal(
                    message.api.clone(),
                    message.provider.clone(),
                    message.model.clone(),
                    StopReason::Aborted,
                    "Operation aborted",
                    crate::clock::now_ms(),
                )));
                // Do not call a provider just to manufacture an abort, or mutate
                // the original tool-call assistant and its settled tool results.
                let queued_count = pending_messages.len();
                for (ordinal, message) in pending_messages
                    .drain(..)
                    .chain(std::iter::once(aborted.clone()))
                    .enumerate()
                {
                    let start = AgentEvent::MessageStart {
                        message_id: None,
                        message: message.clone(),
                    };
                    if ordinal < queued_count {
                        emit.emit_injected(start).await;
                    } else {
                        emit_event(emit, start).await;
                    }
                    current_context.messages.push(message.clone());
                    new_messages.push(message.clone());
                    emit_event(
                        emit,
                        AgentEvent::MessageEnd {
                            message_id: None,
                            message,
                        },
                    )
                    .await;
                }
                emit_event(
                    emit,
                    AgentEvent::TurnEnd {
                        message: aborted,
                        tool_results: Vec::new(),
                        // 上游新加的 abort 收尾路径也要带上排队计数，否则
                        // 远程 UI 在这条路径上看到的是「没有排队」。
                        pending_messages: pending_count(config),
                    },
                )
                .await;
                emit_event(
                    emit,
                    AgentEvent::AgentEnd {
                        messages: new_messages.clone(),
                    },
                )
                .await;
                return Ok(LoopOutcome::Aborted);
            }
        }

        // Agent would stop here. Check for follow-up messages.
        let follow_ups = drain_follow_up(config).await;
        if !follow_ups.is_empty() {
            pending_messages = follow_ups;
            continue;
        }
        break;
    }

    emit_event(
        emit,
        AgentEvent::AgentEnd {
            messages: new_messages.clone(),
        },
    )
    .await;
    Ok(LoopOutcome::Completed)
}

// ----------------------------------------------------------------------------
// streamAssistantResponse
// ----------------------------------------------------------------------------

struct AssistantRequestDiagnostics {
    enabled: bool,
    started: Instant,
    input_messages: usize,
    llm_messages: usize,
    tools: usize,
    transform_ns: u64,
    history_clone_ns: u64,
    context_build_ns: u64,
    convert_ns: u64,
    stream_open_ns: u64,
    stream_started: Option<Instant>,
    first_poll_ns: u64,
    provider_events: u64,
    first_event_ns: u64,
    emit_calls: u64,
    emit_ns: u64,
    fold_ns: u64,
}

impl AssistantRequestDiagnostics {
    fn new(input_messages: usize, tools: usize) -> Self {
        Self {
            enabled: agent_diagnostics_enabled(),
            started: Instant::now(),
            input_messages,
            llm_messages: 0,
            tools,
            transform_ns: 0,
            history_clone_ns: 0,
            context_build_ns: 0,
            convert_ns: 0,
            stream_open_ns: 0,
            stream_started: None,
            first_poll_ns: 0,
            provider_events: 0,
            first_event_ns: 0,
            emit_calls: 0,
            emit_ns: 0,
            fold_ns: 0,
        }
    }

    fn elapsed_ns(started: Instant) -> u64 {
        started.elapsed().as_nanos().min(u64::MAX as u128) as u64
    }

    async fn emit_agent_event(&mut self, emit: &Arc<dyn AgentEmitter>, event: AgentEvent) {
        let started = self.enabled.then(Instant::now);
        emit_event(emit, event).await;
        if let Some(started) = started {
            self.emit_calls += 1;
            self.emit_ns += Self::elapsed_ns(started);
        }
    }
}

impl Drop for AssistantRequestDiagnostics {
    fn drop(&mut self) {
        if !self.enabled {
            return;
        }
        let stream_ns = self
            .stream_started
            .map(Self::elapsed_ns)
            .unwrap_or_default();
        eprintln!(
            "RPI_AGENT_DIAGNOSTICS phase=assistant input_messages={} llm_messages={} tools={} transform_ns={} history_clone_ns={} context_build_ns={} convert_ns={} stream_open_ns={} first_event_ns={} first_poll_ns={} provider_events={} emit_calls={} emit_ns={} fold_ns={} stream_ns={} total_ns={}",
            self.input_messages,
            self.llm_messages,
            self.tools,
            self.transform_ns,
            self.history_clone_ns,
            self.context_build_ns,
            self.convert_ns,
            self.stream_open_ns,
            self.first_event_ns,
            self.first_poll_ns,
            self.provider_events,
            self.emit_calls,
            self.emit_ns,
            self.fold_ns,
            stream_ns,
            Self::elapsed_ns(self.started),
        );
    }
}

struct ProviderMessageCache {
    initialized: bool,
    source_len: usize,
    messages: Vec<Message>,
}

impl ProviderMessageCache {
    fn new() -> Self {
        Self {
            initialized: false,
            source_len: 0,
            messages: Vec::new(),
        }
    }

    fn invalidate(&mut self) {
        self.initialized = false;
        self.source_len = 0;
        self.messages.clear();
    }

    fn take_messages(
        &mut self,
        context: &AgentContext,
        config: &AgentLoopConfig,
    ) -> Option<Vec<Message>> {
        if config.transform_context.is_some() {
            return None;
        }
        let convert = config.convert_to_llm_incremental.as_ref()?;
        // The first request must use the full converter: harness conversion
        // repairs orphaned/interleaved tool history before caching it.
        if !self.initialized {
            return None;
        }
        if self.source_len > context.messages.len() {
            self.invalidate();
            return None;
        }
        let suffix = &context.messages[self.source_len..];
        self.messages.extend(convert(suffix));
        self.source_len = context.messages.len();
        Some(std::mem::take(&mut self.messages))
    }

    fn restore_messages(&mut self, messages: Vec<Message>) {
        self.initialized = true;
        self.messages = messages;
    }
}

/// Stream one assistant response, folding protocol events into the partial
/// message and emitting agent `Message*` events. Mirrors TS
/// `streamAssistantResponse`.
async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    provider_cache: &mut ProviderMessageCache,
    emit: &Arc<dyn AgentEmitter>,
    stream_fn: &StreamFn,
) -> Result<AssistantMessage, AgentError> {
    let mut diagnostics =
        AssistantRequestDiagnostics::new(context.messages.len(), context.tools.len());
    // The incremental path owns the converted provider messages between turns.
    // Moving them into `Context` avoids cloning the full provider history; the
    // synchronous stream factory cannot retain the borrowed context.
    let cached_messages = provider_cache.take_messages(context, config);
    let cache_enabled =
        config.transform_context.is_none() && config.convert_to_llm_incremental.is_some();
    let llm_messages = if let Some(messages) = cached_messages {
        messages
    } else {
        let transform_started = diagnostics.enabled.then(Instant::now);
        let messages = if let Some(transform) = &config.transform_context {
            let clone_started = diagnostics.enabled.then(Instant::now);
            let messages = context.messages.clone();
            if let Some(started) = clone_started {
                diagnostics.history_clone_ns = AssistantRequestDiagnostics::elapsed_ns(started);
            }
            transform(messages, config.signal.clone()).await
        } else {
            let clone_started = diagnostics.enabled.then(Instant::now);
            let messages = context.messages.clone();
            if let Some(started) = clone_started {
                diagnostics.history_clone_ns = AssistantRequestDiagnostics::elapsed_ns(started);
            }
            messages
        };
        if let Some(started) = transform_started {
            diagnostics.transform_ns = AssistantRequestDiagnostics::elapsed_ns(started);
        }
        let convert_started = diagnostics.enabled.then(Instant::now);
        let converted = (config.convert_to_llm)(messages).await;
        if let Some(started) = convert_started {
            diagnostics.convert_ns = AssistantRequestDiagnostics::elapsed_ns(started);
        }
        converted
    };
    diagnostics.llm_messages = llm_messages.len();

    let context_build_started = diagnostics.enabled.then(Instant::now);
    let mut llm_context = rpi_ai::types::Context {
        system_prompt: if context.system_prompt.is_empty() {
            None
        } else {
            Some(context.system_prompt.clone())
        },
        messages: llm_messages,
        tools: context.tools.iter().map(|t| t.schema().clone()).collect(),
    };
    if let Some(started) = context_build_started {
        diagnostics.context_build_ns = AssistantRequestDiagnostics::elapsed_ns(started);
    }

    // Resolve API key: getApiKey(provider) ?? config.api_key.
    let resolved_api_key = if let Some(get_key) = &config.get_api_key {
        get_key(&config.model.provider)
            .await
            .or_else(|| config.api_key.clone())
    } else {
        config.api_key.clone()
    };

    let opts = config.to_stream_options(resolved_api_key);
    let stream_open_started = diagnostics.enabled.then(Instant::now);
    let mut response = stream_fn(&config.model, &llm_context, &opts);
    if cache_enabled {
        provider_cache.source_len = context.messages.len();
        provider_cache.restore_messages(std::mem::take(&mut llm_context.messages));
    }
    if let Some(started) = stream_open_started {
        diagnostics.stream_open_ns = AssistantRequestDiagnostics::elapsed_ns(started);
        diagnostics.stream_started = Some(Instant::now());
    }

    let mut added_partial = false;
    let mut first_poll = true;

    loop {
        let poll_started = diagnostics.enabled.then(Instant::now);
        let next = response.next().await;
        let poll_ns = poll_started.map(AssistantRequestDiagnostics::elapsed_ns);
        let Some(event) = next else {
            break;
        };
        let event_started = diagnostics.enabled.then(Instant::now);
        if diagnostics.enabled {
            diagnostics.provider_events += 1;
            if first_poll {
                diagnostics.first_poll_ns = poll_ns.unwrap_or_default();
                if let Some(stream_started) = diagnostics.stream_started {
                    diagnostics.first_event_ns =
                        AssistantRequestDiagnostics::elapsed_ns(stream_started);
                }
                first_poll = false;
            }
        }
        match &event {
            AssistantMessageEvent::Start { partial } => {
                let am = AgentMessage::Assistant(Box::new((**partial).clone()));
                context.messages.push(am.clone());
                added_partial = true;
                diagnostics
                    .emit_agent_event(
                        emit,
                        AgentEvent::MessageStart {
                            message_id: None,
                            message: am,
                        },
                    )
                    .await;
            }
            AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolCallStart { partial, .. }
            | AssistantMessageEvent::ToolCallDelta { partial, .. }
            | AssistantMessageEvent::ToolCallEnd { partial, .. } => {
                if added_partial {
                    // No `context.messages` update here: nothing reads it between
                    // deltas (`transform_context`/`convert_to_llm` ran before this
                    // loop), and `Start` already pushed the slot while the
                    // terminal arm replaces it with the finalized message. Keeping
                    // the old `*last = am.clone()` copied the whole growing message
                    // once per delta for no observable effect.
                    diagnostics
                        .emit_agent_event(
                            emit,
                            AgentEvent::MessageUpdate {
                                message_id: None,
                                // `Arc` bump, not a deep clone: this fires once per
                                // streaming delta, and the provider already built the
                                // snapshot it hands us.
                                message: Arc::clone(partial),
                                assistant_message_event: event,
                            },
                        )
                        .await;
                }
            }
            AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => {
                let final_message = response.result().await.map_err(|_| {
                    AgentError::Provider(
                        "assistant-message event stream ended without a terminal event".into(),
                    )
                })?;
                let am = AgentMessage::Assistant(Box::new(final_message.clone()));
                if added_partial {
                    if let Some(last) = context.messages.last_mut() {
                        *last = am.clone();
                    }
                } else {
                    context.messages.push(am.clone());
                    diagnostics
                        .emit_agent_event(
                            emit,
                            AgentEvent::MessageStart {
                                message_id: None,
                                message: am.clone(),
                            },
                        )
                        .await;
                }
                diagnostics
                    .emit_agent_event(
                        emit,
                        AgentEvent::MessageEnd {
                            message_id: None,
                            message: am,
                        },
                    )
                    .await;
                if let Some(started) = event_started {
                    diagnostics.fold_ns += AssistantRequestDiagnostics::elapsed_ns(started);
                }
                return Ok(final_message);
            }
        }
        if let Some(started) = event_started {
            diagnostics.fold_ns += AssistantRequestDiagnostics::elapsed_ns(started);
        }
    }

    // Stream ended without a terminal event — finalize from result() (TS has the
    // same fallback).
    let final_message = response.result().await.map_err(|_| {
        AgentError::Provider("assistant-message event stream ended without a terminal event".into())
    })?;
    let am = AgentMessage::Assistant(Box::new(final_message.clone()));
    if added_partial {
        if let Some(last) = context.messages.last_mut() {
            *last = am.clone();
        }
    } else {
        context.messages.push(am.clone());
        diagnostics
            .emit_agent_event(
                emit,
                AgentEvent::MessageStart {
                    message_id: None,
                    message: am.clone(),
                },
            )
            .await;
    }
    diagnostics
        .emit_agent_event(
            emit,
            AgentEvent::MessageEnd {
                message_id: None,
                message: am,
            },
        )
        .await;
    Ok(final_message)
}

// ----------------------------------------------------------------------------
// Truncate-fail
// ----------------------------------------------------------------------------

/// Reject an ambiguous provider batch before ANY invocation starts. A duplicate
/// call ID cannot safely correlate permission, execution, or durable result
/// reservations, even if one of its calls would be rejected independently.
async fn reject_ambiguous_tool_batch(
    tool_calls: &[ToolCall],
    emit: &Arc<dyn AgentEmitter>,
) -> Option<ExecutedToolBatch> {
    let mut ids = std::collections::BTreeSet::new();
    if tool_calls.iter().all(|call| ids.insert(call.id.as_str())) {
        return None;
    }
    let mut messages = Vec::with_capacity(tool_calls.len());
    for call in tool_calls {
        let finalized = FinalizedToolCall {
            tool_call: call.clone(),
            result: create_error_tool_result(
                "Tool batch was rejected: provider returned duplicate tool call IDs",
            ),
            is_error: true,
        };
        emit_tool_execution_end(emit, &finalized).await;
        let result = create_tool_result_message(call, &finalized.result, true);
        emit_tool_result_message(emit, &result).await;
        messages.push(result);
    }
    Some(ExecutedToolBatch {
        messages,
        terminate: false,
    })
}

/// Fail every tool call in a truncated message. Mirrors TS
/// `failToolCallsFromTruncatedMessage`. Each call gets a `tool_execution_start`
/// + `tool_execution_end` (is_error:true) + tool-result `MessageStart`/`End`,
/// but is *not* executed.
async fn fail_tool_calls_from_truncated_message(
    tool_calls: &[ToolCall],
    emit: &Arc<dyn AgentEmitter>,
) -> Result<ExecutedToolBatch, AgentError> {
    if let Some(rejected) = reject_ambiguous_tool_batch(tool_calls, emit).await {
        return Ok(rejected);
    }
    let mut messages: Vec<ToolResultMessage> = Vec::new();
    for tool_call in tool_calls {
        emit_event(
            emit,
            AgentEvent::ToolExecutionStart {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                args: tool_call.arguments.clone(),
            },
        )
        .await;
        let reason = format!(
            "Tool call {:?} was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
            tool_call.name
        );
        let result = create_error_tool_result(&reason);
        emit_event(
            emit,
            AgentEvent::ToolExecutionEnd {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                result: result.clone(),
                is_error: true,
            },
        )
        .await;
        let trm = create_tool_result_message(tool_call, &result, true);
        emit_tool_result_message(emit, &trm).await;
        messages.push(trm);
    }
    Ok(ExecutedToolBatch {
        messages,
        terminate: false,
    })
}

// ----------------------------------------------------------------------------
// Tool-call execution
// ----------------------------------------------------------------------------

/// Execute a batch of tool calls. Sequential if `config.tool_execution ==
/// Sequential` or any matched tool's `execution_mode()` is `Sequential`;
/// otherwise parallel. Mirrors TS `executeToolCalls`.
async fn execute_tool_calls(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    emit: &Arc<dyn AgentEmitter>,
) -> Result<ExecutedToolBatch, AgentError> {
    if let Some(rejected) = reject_ambiguous_tool_batch(tool_calls, emit).await {
        return Ok(rejected);
    }
    let has_sequential = tool_calls.iter().any(|tc| {
        current_context
            .tools
            .iter()
            .find(|t| t.schema().name == tc.name)
            .map(|t| t.execution_mode() == ToolExecutionMode::Sequential)
            .unwrap_or(false)
    });
    if config.tool_execution == ToolExecutionMode::Sequential || has_sequential {
        execute_tool_calls_sequential(current_context, assistant_message, tool_calls, config, emit)
            .await
    } else {
        execute_tool_calls_parallel(current_context, assistant_message, tool_calls, config, emit)
            .await
    }
}

async fn execute_tool_calls_sequential(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    emit: &Arc<dyn AgentEmitter>,
) -> Result<ExecutedToolBatch, AgentError> {
    // One immutable snapshot per batch, shared by every capability/descendant.
    let current_context = &Arc::new(current_context.clone());
    let assistant_message = &Arc::new(assistant_message.clone());
    let mut finalized_calls: Vec<FinalizedToolCall> = Vec::new();
    let mut messages: Vec<ToolResultMessage> = Vec::new();

    for tool_call in tool_calls {
        if let Some(reason) = invalid_tool_call_reason(tool_call) {
            let finalized = FinalizedToolCall {
                tool_call: tool_call.clone(),
                result: create_error_tool_result(&reason),
                is_error: true,
            };
            emit_tool_execution_end(emit, &finalized).await;
            let trm = create_tool_result_message(tool_call, &finalized.result, true);
            emit_tool_result_message(emit, &trm).await;
            messages.push(trm);
            finalized_calls.push(finalized);
            continue;
        }
        emit_event(
            emit,
            AgentEvent::ToolExecutionStart {
                tool_call_id: tool_call.id.clone(),
                tool_name: tool_call.name.clone(),
                args: tool_call.arguments.clone(),
            },
        )
        .await;

        let finalized =
            run_one_tool_call(current_context, assistant_message, tool_call, config, emit).await?;

        emit_tool_execution_end(emit, &finalized).await;
        let trm =
            create_tool_result_message(&finalized.tool_call, &finalized.result, finalized.is_error);
        emit_tool_result_message(emit, &trm).await;
        finalized_calls.push(finalized);
        messages.push(trm);

        if config.signal.is_cancelled() {
            break;
        }
    }

    Ok(ExecutedToolBatch {
        messages,
        terminate: should_terminate_tool_batch(&finalized_calls),
    })
}

/// Parallel execution. Mirrors TS `executeToolCallsParallel`:
/// 1. Each call gets `tool_execution_start` + is prepared sequentially
///    (prepare may mutate args / block).
/// 2. Immediate outcomes (not-found / blocked / aborted / validation error)
///    emit `tool_execution_end` immediately.
/// 3. Ready calls run concurrently. **`tool_execution_end` is emitted in
///    COMPLETION order** — we drive all prepared futures with `join_set`-style
///    polling and emit as each resolves.
/// 4. After all settle, **tool-result `MessageStart`/`MessageEnd` are emitted in
///    SOURCE/ordinal order** — we walk the finalized vec by index.
async fn execute_tool_calls_parallel(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    emit: &Arc<dyn AgentEmitter>,
) -> Result<ExecutedToolBatch, AgentError> {
    // The loop owns these futures: dropping it drops execute/finalize too,
    // rather than detaching live tasks that recovery could replay concurrently.
    let mut running = RunningToolCalls::new();
    let mut completed = VecDeque::new();
    let mut finalized_by_index = Vec::with_capacity(tool_calls.len());
    let execution_context = (
        Arc::new(current_context.clone()),
        Arc::new(assistant_message.clone()),
    );

    for tool_call in tool_calls {
        let index = finalized_by_index.len();
        finalized_by_index.push(None);
        let preparation = async {
            if let Some(reason) = invalid_tool_call_reason(tool_call) {
                return Prepared::Immediate {
                    result: create_error_tool_result(&reason),
                    is_error: true,
                };
            }
            emit_event(
                emit,
                AgentEvent::ToolExecutionStart {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    args: tool_call.arguments.clone(),
                },
            )
            .await;
            prepare_tool_call(current_context, assistant_message, tool_call, config).await
        };
        // Preparation remains sequential, but ready peers must keep running
        // while a later authorization/preparation hook is awaiting them.
        match drive_tool_calls_while(preparation, &mut running, &mut completed).await {
            Prepared::Immediate { result, is_error } => {
                let finalized = FinalizedToolCall {
                    tool_call: tool_call.clone(),
                    result,
                    is_error,
                };
                drive_tool_calls_while(
                    emit_tool_execution_end(emit, &finalized),
                    &mut running,
                    &mut completed,
                )
                .await;
                finalized_by_index[index] = Some(finalized);
            }
            Prepared::Ready { tool, args } => {
                let tc = tool_call.clone();
                let panic_call = tc.clone();
                let (ctx, am) = (
                    Arc::clone(&execution_context.0),
                    Arc::clone(&execution_context.1),
                );
                let cfg = config.clone();
                let emit = Arc::clone(emit);
                let execute = std::panic::AssertUnwindSafe(async move {
                    let executed =
                        execute_prepared_tool_call(&tc, &tool, &args, &cfg, &emit, &ctx, &am).await;
                    finalize_executed_tool_call(&ctx, &am, &tc, &args, executed, &cfg).await
                })
                .catch_unwind();
                running.push(Box::pin(async move {
                    let finalized = execute.await.unwrap_or_else(|_| FinalizedToolCall {
                        tool_call: panic_call,
                        result: create_error_tool_result("tool task panicked"),
                        is_error: true,
                    });
                    (index, finalized)
                }));
            }
        }
        if config.signal.is_cancelled() {
            break;
        }
    }

    // Completion events retain completion order, including peers that finished
    // during preparation or while a backpressured event was being delivered.
    loop {
        let next = match completed.pop_front() {
            Some(next) => Some(next),
            None => running.next().await,
        };
        let Some((index, finalized)) = next else {
            break;
        };
        drive_tool_calls_while(
            emit_tool_execution_end(emit, &finalized),
            &mut running,
            &mut completed,
        )
        .await;
        finalized_by_index[index] = Some(finalized);
    }

    // Tool-result messages still settle in source/ordinal order.
    let mut messages = Vec::new();
    let mut finalized_calls = Vec::new();
    for slot in finalized_by_index {
        let finalized = slot.expect("every tool call finalized");
        let trm =
            create_tool_result_message(&finalized.tool_call, &finalized.result, finalized.is_error);
        emit_tool_result_message(emit, &trm).await;
        messages.push(trm);
        finalized_calls.push(finalized);
    }
    Ok(ExecutedToolBatch {
        messages,
        terminate: should_terminate_tool_batch(&finalized_calls),
    })
}

type RunningToolCalls = FuturesUnordered<BoxFuture<'static, (usize, FinalizedToolCall)>>;

/// Drive owned execute futures even while preparation/event delivery waits.
/// Completed calls queue up for ordered event delivery; no tool task detaches.
async fn drive_tool_calls_while<F: std::future::Future>(
    future: F,
    running: &mut RunningToolCalls,
    completed: &mut VecDeque<(usize, FinalizedToolCall)>,
) -> F::Output {
    tokio::pin!(future);
    loop {
        tokio::select! {
            output = &mut future => return output,
            Some(result) = running.next(), if !running.is_empty() => {
                completed.push_back(result);
            }
        }
    }
}

/// Shared core for the sequential path: prepare → execute → finalize.
pub(crate) async fn run_one_tool_call(
    current_context: &Arc<AgentContext>,
    assistant_message: &Arc<AssistantMessage>,
    tool_call: &ToolCall,
    config: &AgentLoopConfig,
    emit: &Arc<dyn AgentEmitter>,
) -> Result<FinalizedToolCall, AgentError> {
    match prepare_tool_call(current_context, assistant_message, tool_call, config).await {
        Prepared::Immediate { result, is_error } => Ok(FinalizedToolCall {
            tool_call: tool_call.clone(),
            result,
            is_error,
        }),
        Prepared::Ready { tool, args } => {
            let executed = execute_prepared_tool_call(
                tool_call,
                &tool,
                &args,
                config,
                emit,
                current_context,
                assistant_message,
            )
            .await;
            Ok(finalize_executed_tool_call(
                current_context,
                assistant_message,
                tool_call,
                &args,
                executed,
                config,
            )
            .await)
        }
    }
}

/// Outcome of [`prepare_tool_call`].
enum Prepared {
    /// Resolved without executing (not-found / blocked / aborted / validation error).
    Immediate {
        result: AgentToolResult,
        is_error: bool,
    },
    /// Validated and ready to execute.
    Ready {
        tool: Arc<dyn AgentTool>,
        args: serde_json::Value,
    },
}

/// Find the tool, call `prepare_arguments`, validate args, run `before_tool_call`.
/// Mirrors TS `prepareToolCall`.
async fn prepare_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    config: &AgentLoopConfig,
) -> Prepared {
    // A provider may begin a streamed tool block with empty metadata, but only
    // the terminal tool call is executable. Never let an incomplete id/name
    // reach a plugin approval hook or a real tool: an empty id cannot be
    // correlated with its result and can leave the harness active forever.
    if let Some(reason) = invalid_tool_call_reason(tool_call) {
        return Prepared::Immediate {
            result: create_error_tool_result(&reason),
            is_error: true,
        };
    }

    // A recorded call already ran upstream. Its recording is the result, and the
    // call must never reach a tool, a hook or an approval prompt: any path to
    // `execute` would run it a second time. An unusable recording is an error
    // result, which is also not an execution.
    if let Some(recording) = &tool_call.recorded {
        let (result, is_error) = match recorded_tool_result(recording) {
            Ok(decoded) => decoded,
            Err(reason) => (create_error_tool_result(&reason), true),
        };
        return Prepared::Immediate { result, is_error };
    }

    let tool = current_context
        .tools
        .iter()
        .find(|t| t.schema().name == tool_call.name)
        .cloned();
    let tool = match tool {
        Some(t) => t,
        None => {
            return Prepared::Immediate {
                result: create_error_tool_result(&format!("Tool {} not found", tool_call.name)),
                is_error: true,
            };
        }
    };

    // prepareArguments + schema validation.
    let prepared_args = match tool.prepare_arguments(tool_call.arguments.clone()) {
        Ok(v) => v,
        Err(e) => {
            return Prepared::Immediate {
                result: create_error_tool_result(&e.to_string()),
                is_error: true,
            };
        }
    };
    let mut prepared_tool_call = tool_call.clone();
    prepared_tool_call.arguments = prepared_args;

    let validated_args = match validate_tool_arguments(tool.schema(), &prepared_tool_call) {
        Ok(v) => v,
        Err(e) => {
            return Prepared::Immediate {
                result: create_error_tool_result(&e.to_string()),
                is_error: true,
            };
        }
    };

    // before_tool_call hook (may block + set terminate, may replace args).
    // TS hands the callback `args` by reference and lets JS mutate it in place;
    // Rust hands an immutable borrow, so a rewrite is signalled by
    // `BeforeToolCallResult::args`. The replacement is applied WITHOUT
    // re-validation, mirroring TS where the mutation lands after
    // `validateToolArguments` and is trusted.
    let mut validated_args = validated_args;
    if let Some(before) = &config.before_tool_call {
        let ctx = BeforeToolCallContext {
            assistant_message,
            tool_call: &prepared_tool_call,
            args: &validated_args,
            context: current_context,
        };
        let before_result = before(ctx, config.signal.clone()).await;
        if config.signal.is_cancelled() {
            return Prepared::Immediate {
                result: create_error_tool_result("Operation aborted"),
                is_error: true,
            };
        }
        if let Some(br) = before_result {
            if let Some(replacement) = br.args {
                validated_args = replacement;
            }
            if br.block {
                let mut result = create_error_tool_result(
                    &br.reason
                        .unwrap_or_else(|| "Tool execution was blocked".to_string()),
                );
                if br.terminate {
                    result.terminate = true;
                }
                return Prepared::Immediate {
                    result,
                    is_error: true,
                };
            }
        }
    }

    if config.signal.is_cancelled() {
        return Prepared::Immediate {
            result: create_error_tool_result("Operation aborted"),
            is_error: true,
        };
    }

    Prepared::Ready {
        tool,
        args: validated_args,
    }
}

/// Reject provider tool-call records that cannot be correlated or dispatched.
fn invalid_tool_call_reason(tool_call: &ToolCall) -> Option<String> {
    if tool_call.id.trim().is_empty() {
        return Some(format!(
            "Tool call for {:?} was rejected: provider returned an empty tool-call id",
            tool_call.name
        ));
    }
    if tool_call.name.trim().is_empty() {
        return Some("Tool call was rejected: provider returned an empty tool name".into());
    }
    None
}

/// Decode the `recorded` field of a streamed tool call into the tool result it
/// stands for, plus whether that result is an error.
///
/// The accepted shape is
/// `{"version":1,"isError":bool,"result":{"content":[...],"details":any,"terminate":bool}}`,
/// which is what the Cursor replay worker emits. `result.content` holds `text`
/// or `image` blocks. `details` and `terminate` are optional; `terminate` sets
/// `AgentToolResult::terminate`, so a recorded batch can stop the loop the same
/// way a live one does. Other fields inside `result` are ignored. A partial
/// recording such as `{"version":1}` is rejected as invalid. Any other version
/// is reported as unsupported. Callers answer a rejected recording with an error
/// result and never execute the call.
pub fn recorded_tool_result(
    recording: &serde_json::Value,
) -> Result<(AgentToolResult, bool), String> {
    const INVALID: &str =
        "Recorded tool result is unavailable or invalid; replay cannot execute work";
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Recording {
        // Checked above; present here so `deny_unknown_fields` accepts it.
        _version: u64,
        is_error: bool,
        result: RecordedResult,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RecordedResult {
        content: Vec<Content>,
        #[serde(default)]
        details: serde_json::Value,
        #[serde(default)]
        terminate: bool,
    }
    match recording.get("version").and_then(serde_json::Value::as_u64) {
        Some(1) => {}
        Some(_) => {
            return Err(
                "Unsupported recorded tool result version; replay cannot execute work".into(),
            );
        }
        None => return Err(INVALID.into()),
    }
    let recording: Recording =
        serde_json::from_value(recording.clone()).map_err(|_| INVALID.to_string())?;
    let result = recording.result;
    let content = result
        .content
        .into_iter()
        .map(|block| match block {
            Content::Text(text) => Ok(TextContentOrImage::Text(text)),
            Content::Image(image) => Ok(TextContentOrImage::Image(image)),
            _ => Err(INVALID.to_string()),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let result = AgentToolResult {
        content,
        details: result.details,
        usage: None,
        added_tool_names: Vec::new(),
        terminate: result.terminate,
    };
    Ok((result, recording.is_error))
}

#[cfg(test)]
mod invalid_tool_call_tests {
    use super::invalid_tool_call_reason;
    use rpi_ai::types::{ToolCall, ToolCallType};

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            kind: ToolCallType,
            id: id.into(),
            name: name.into(),
            arguments: serde_json::json!({}),
            thought_signature: None,
            namespace: None,
            recorded: None,
        }
    }

    #[test]
    fn rejects_empty_id_or_name_but_accepts_complete_call() {
        assert!(invalid_tool_call_reason(&call("", "bash")).is_some());
        assert!(invalid_tool_call_reason(&call("call-1", "")).is_some());
        assert!(invalid_tool_call_reason(&call("call-1", "bash")).is_none());
    }
}

/// Close the update gate even if execute unwinds or its owner is dropped.
struct ToolUpdateGuard(Arc<AtomicBool>);

impl Drop for ToolUpdateGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Run the tool's `execute` with late-update suppression. Mirrors TS
/// `executePreparedToolCall`.
async fn execute_prepared_tool_call(
    tool_call: &ToolCall,
    tool: &Arc<dyn AgentTool>,
    args: &serde_json::Value,
    config: &AgentLoopConfig,
    emit: &Arc<dyn AgentEmitter>,
    current_context: &Arc<AgentContext>,
    assistant_message: &Arc<AssistantMessage>,
) -> ExecutedToolCallOutcome {
    let child_token = config.signal.child_token();
    // Retire the native invocation after execute (including adapter image
    // postprocessing) and descendant drain, not when its program binding closes.
    let _native_lifetime = crate::tool_program::InvocationLifetime(child_token.clone());
    // Gate for late updates: flipped false once execute resolves. on_update
    // checks it and returns early. This is the late-update-suppression invariant.
    let accepting_updates = Arc::new(AtomicBool::new(true));
    let _update_guard = ToolUpdateGuard(Arc::clone(&accepting_updates));
    let tool_call_id = tool_call.id.clone();
    let tool_name = tool_call.name.clone();
    let diagnostic_tool_name = tool_name.clone();
    let args_clone = args.clone();
    let emit_clone = Arc::clone(emit);

    let on_update: Arc<dyn Fn(crate::types::ToolResultPartial) + Send + Sync> = {
        let gate = Arc::clone(&accepting_updates);
        let update_signal = child_token.clone();
        Arc::new(move |partial: crate::types::ToolResultPartial| {
            if !gate.load(Ordering::SeqCst) || update_signal.is_cancelled() {
                return;
            }
            // Both completion and cancellation retire the native update callback.
            let ev = AgentEvent::ToolExecutionUpdate {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                args: args_clone.clone(),
                partial_result: Arc::new(partial),
            };
            // `try_emit` is the non-blocking sync surface, so `on_update` never
            // awaits (it's an `Arc<dyn Fn>`, not an async). Late-update
            // suppression + ordering: update events interleave correctly because
            // they share the collector's mutex / the broadcast's channel.
            emit_clone.try_emit(ev);
        })
    };

    let capability = crate::tool_program::ToolProgram::capture(
        current_context,
        assistant_message,
        tool_call,
        config,
        child_token.clone(),
        on_update.clone(),
        emit,
    );
    let _lifetime = crate::tool_program::InvocationLifetime(capability.signal.clone());
    let execute_started = agent_diagnostics_enabled().then(Instant::now);
    let executed = std::panic::AssertUnwindSafe(async {
        crate::tool_program::scope(
            capability.clone(),
            tool.execute(&tool_call.id, args.clone(), child_token, on_update),
        )
        .await
    })
    .catch_unwind()
    .await;
    accepting_updates.store(false, Ordering::SeqCst);
    let mut outcome = match executed {
        Ok(Ok(result)) => ExecutedToolCallOutcome {
            result,
            is_error: false,
        },
        Ok(Err(AgentError::StructuredTool(result))) => ExecutedToolCallOutcome {
            result: *result,
            is_error: true,
        },
        Ok(Err(e)) => ExecutedToolCallOutcome {
            result: create_error_tool_result(&e.to_string()),
            is_error: true,
        },
        Err(panic) => {
            // Keep the containing request's gate/permits until every descendant
            // has stopped, even when execute unwinds. Then let the existing
            // outer panic handler preserve its error and hook semantics.
            capability.finish().await;
            std::panic::resume_unwind(panic);
        }
    };
    if let Some(started) = execute_started {
        eprintln!(
            "RPI_TOOL_DIAGNOSTICS tool={} call_id={} is_error={} execute_ns={}",
            diagnostic_tool_name,
            tool_call.id,
            outcome.is_error,
            started.elapsed().as_nanos().min(u64::MAX as u128),
        );
    }
    capability.finish().await;
    if let Err(error) = capability.merge_accounting(&mut outcome.result) {
        outcome.is_error = true;
        outcome.result.content.push(TextContentOrImage::text(error));
    }
    outcome
}

/// Result of `execute` before `after_tool_call` overrides. Mirrors TS
/// `ExecutedToolCallOutcome`.
struct ExecutedToolCallOutcome {
    result: AgentToolResult,
    is_error: bool,
}

/// Apply `after_tool_call` overrides. Mirrors TS `finalizeExecutedToolCall`.
async fn finalize_executed_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &ToolCall,
    args: &serde_json::Value,
    executed: ExecutedToolCallOutcome,
    config: &AgentLoopConfig,
) -> FinalizedToolCall {
    let mut result = executed.result;
    let mut is_error = executed.is_error;

    if let Some(after) = &config.after_tool_call {
        let ctx = AfterToolCallContext {
            assistant_message,
            tool_call,
            args,
            result: &result,
            is_error,
            context: current_context,
        };
        match after(ctx, config.signal.clone()).await {
            Some(after_result) => {
                if let Some(c) = after_result.content {
                    result.content = c;
                }
                if let Some(d) = after_result.details {
                    result.details = d;
                }
                if let Some(u) = after_result.usage {
                    result.usage = Some(u);
                }
                if let Some(t) = after_result.terminate {
                    result.terminate = t;
                }
                if let Some(ie) = after_result.is_error {
                    is_error = ie;
                }
            }
            None => {}
        }
    }

    FinalizedToolCall {
        tool_call: tool_call.clone(),
        result,
        is_error,
    }
}

// ----------------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------------

/// Early-terminate iff the batch is non-empty AND every result sets
/// `terminate == true`. Mirrors TS `shouldTerminateToolBatch`.
fn should_terminate_tool_batch(finalized_calls: &[FinalizedToolCall]) -> bool {
    !finalized_calls.is_empty() && finalized_calls.iter().all(|f| f.result.terminate)
}

/// Build an error `AgentToolResult` — text content, null details.
/// Mirrors TS `createErrorToolResult`.
fn create_error_tool_result(message: &str) -> AgentToolResult {
    AgentToolResult::error_text(message)
}

/// Build a `ToolResultMessage` from a finalized call. Mirrors TS
/// `createToolResultMessage`. Content is normalized to non-null via
/// `AgentToolResult::into_content` (TS guards `result.content ?? []`).
fn create_tool_result_message(
    tool_call: &ToolCall,
    result: &AgentToolResult,
    is_error: bool,
) -> ToolResultMessage {
    ToolResultMessage {
        role: ToolResultRole,
        tool_call_id: tool_call.id.clone(),
        tool_name: tool_call.name.clone(),
        content: result.clone().into_content(),
        details: Some(result.details.clone()),
        usage: result.usage.clone(),
        added_tool_names: result.added_tool_names.clone(),
        is_error,
        timestamp: crate::clock::now_ms(),
    }
}

/// Emit `tool_execution_end` for a finalized call.
async fn emit_tool_execution_end(emit: &Arc<dyn AgentEmitter>, finalized: &FinalizedToolCall) {
    emit_event(
        emit,
        AgentEvent::ToolExecutionEnd {
            tool_call_id: finalized.tool_call.id.clone(),
            tool_name: finalized.tool_call.name.clone(),
            result: finalized.result.clone(),
            is_error: finalized.is_error,
        },
    )
    .await;
}

/// Emit `message_start` + `message_end` for a tool-result message. Tool-result
/// messages fire AFTER all `tool_execution_end`s, in source/ordinal order.
async fn emit_tool_result_message(emit: &Arc<dyn AgentEmitter>, trm: &ToolResultMessage) {
    let am = AgentMessage::ToolResult(Box::new(trm.clone()));
    emit_event(
        emit,
        AgentEvent::MessageStart {
            message_id: None,
            message: am.clone(),
        },
    )
    .await;
    emit_event(
        emit,
        AgentEvent::MessageEnd {
            message_id: None,
            message: am,
        },
    )
    .await;
}

/// Drain steering messages (empty vec if no hook).
async fn drain_steering(config: &AgentLoopConfig) -> Vec<AgentMessage> {
    if let Some(hook) = &config.get_steering_messages {
        hook().await
    } else {
        Vec::new()
    }
}

/// How many messages are still queued (steering + follow-up), **without draining**.
///
/// Must not use `drain_steering` / `drain_follow_up`: those consume the queue.
/// A host that does not install the hook reports 0 — same as "nothing queued".
fn pending_count(config: &AgentLoopConfig) -> usize {
    config
        .pending_message_count
        .as_ref()
        .map(|f| f())
        .unwrap_or(0)
}

/// Drain follow-up messages (empty vec if no hook).
async fn drain_follow_up(config: &AgentLoopConfig) -> Vec<AgentMessage> {
    if let Some(hook) = &config.get_follow_up_messages {
        hook().await
    } else {
        Vec::new()
    }
}

/// Call `prepare_next_turn` if configured.
async fn prepare_next_turn(
    config: &AgentLoopConfig,
    message: &AssistantMessage,
    tool_results: &[ToolResultMessage],
    context: &AgentContext,
    new_messages: &[AgentMessage],
) -> Option<crate::types::AgentLoopTurnUpdate> {
    if let Some(hook) = &config.prepare_next_turn {
        let ctx = crate::types::ShouldStopAfterTurnContext {
            message,
            tool_results,
            context,
            new_messages,
        };
        hook(ctx).await
    } else {
        None
    }
}

async fn after_tool_results(
    config: &AgentLoopConfig,
    message: &AssistantMessage,
    tool_results: &[ToolResultMessage],
    context: &AgentContext,
    new_messages: &[AgentMessage],
) -> Option<crate::types::AgentLoopTurnUpdate> {
    let hook = config.after_tool_results.as_ref()?;
    let ctx = crate::types::ShouldStopAfterTurnContext {
        message,
        tool_results,
        context,
        new_messages,
    };
    hook(ctx).await
}

/// Call `should_stop_after_turn` if configured.
async fn should_stop_after_turn(
    config: &AgentLoopConfig,
    message: &AssistantMessage,
    tool_results: &[ToolResultMessage],
    context: &AgentContext,
    new_messages: &[AgentMessage],
) -> bool {
    if let Some(hook) = &config.should_stop_after_turn {
        let ctx = crate::types::ShouldStopAfterTurnContext {
            message,
            tool_results,
            context,
            new_messages,
        };
        hook(ctx).await
    } else {
        false
    }
}

/// Emit one event via the emitter.
async fn emit_event(emit: &Arc<dyn AgentEmitter>, event: AgentEvent) {
    emit.emit(event).await;
}
