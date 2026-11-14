//! Shared helpers for `pi-agent` integration tests. Mirrors the
//! `MockAssistantStream` + `queueMicrotask` pattern from the TS
//! `agent-loop.test.ts`/`agent.test.ts`: a [`mock_stream_fn`] that returns a
//! scripted `AssistantMessage` per LLM call, emitting a `Start` + `Done` (or
//! `Error`) so the loop's `stream_assistant_response` folds it into the normal
//! `MessageStart`/`MessageEnd` event pair.
//!
//! Included into each test binary via `#[path = "common/mod.rs"] mod common;`.
//! Helpers here are shared across several test binaries; any given binary may
//! only use a subset, so dead-code warnings are suppressed at the module level.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use rpi_agent::StreamFn;
use rpi_ai::event_stream::create_assistant_message_event_stream;
use rpi_ai::types::{
    Api, AssistantMessage, AssistantMessageEvent, Content, DoneReason, ErrorReason, Message,
    StopReason, Tool, ToolCall, ToolCallType,
};

/// Build a mock `StreamFn` that shifts one scripted `AssistantMessage` per call.
///
/// Mirrors the TS mock: each `queueMicrotask` pushes a single terminal event.
/// Here the producer task pushes `Start` (carrying the partial) then `Done`
/// (success subset of `stop_reason`) or `Error` (failure subset). Exhausting the
/// script yields an `Error` event so a miscounted test fails loudly rather than
/// hanging.
pub fn mock_stream_fn(messages: Vec<AssistantMessage>) -> StreamFn {
    let queue: Arc<Mutex<VecDeque<AssistantMessage>>> = Arc::new(Mutex::new(messages.into()));
    rpi_agent::stream_fn(move |_model, _ctx, _opts| {
        let (mut prod, stream) = create_assistant_message_event_stream();
        let q = Arc::clone(&queue);
        tokio::spawn(async move {
            let next = q.lock().expect("mock queue lock").pop_front();
            match next {
                Some(message) => {
                    let partial = Arc::new(message.clone());
                    prod.push(AssistantMessageEvent::Start {
                        partial: partial.clone(),
                    })
                    .await;
                    match message.stop_reason {
                        StopReason::Error | StopReason::Aborted => {
                            let reason = matches!(message.stop_reason, StopReason::Aborted)
                                .then_some(ErrorReason::Aborted)
                                .unwrap_or(ErrorReason::Error);
                            prod.push(AssistantMessageEvent::Error {
                                reason,
                                error: message,
                            })
                            .await;
                        }
                        _ => {
                            let reason = done_reason_from_stop(message.stop_reason);
                            prod.push(AssistantMessageEvent::Done { reason, message })
                                .await;
                        }
                    }
                }
                None => {
                    let err = AssistantMessage::terminal(
                        Api::Other("mock".into()),
                        "mock",
                        "mock",
                        StopReason::Error,
                        "No more mock responses queued",
                        0,
                    );
                    prod.push(AssistantMessageEvent::Error {
                        reason: ErrorReason::Error,
                        error: err,
                    })
                    .await;
                }
            }
        });
        stream
    })
}

fn done_reason_from_stop(stop: StopReason) -> DoneReason {
    match stop {
        StopReason::Stop => DoneReason::Stop,
        StopReason::Length => DoneReason::Length,
        StopReason::ToolUse => DoneReason::ToolUse,
        StopReason::Deferred => DoneReason::Deferred,
        // Error/Aborted/Pending are routed through the Error arm by the caller.
        _ => DoneReason::Stop,
    }
}

/// A mock model with the `openai-responses` api (matches the TS `createModel`).
pub fn mock_model() -> rpi_ai::Model {
    rpi_ai::Model::new(
        "mock",
        "mock",
        Api::Other("openai-responses".into()),
        "mock",
        "https://example.invalid",
    )
}

/// `createUserMessage(text)` — mirrors the TS helper.
pub fn user_message(text: impl Into<String>) -> rpi_agent::AgentMessage {
    rpi_agent::AgentMessage::User(rpi_ai::types::UserMessage::new(
        rpi_ai::types::UserContent::Text(text.into()),
        0,
    ))
}

/// Build an assistant message carrying a single text block. Mirrors TS
/// `createAssistantMessage([{type:"text",text}])`.
pub fn assistant_text(text: impl Into<String>, stop: StopReason) -> AssistantMessage {
    AssistantMessage {
        role: rpi_ai::types::AssistantRole,
        content: vec![Content::text(text)],
        api: Api::Other("openai-responses".into()),
        provider: "mock".to_string(),
        model: "mock".to_string(),
        response_model: None,
        response_id: None,
        usage: rpi_ai::types::Usage::zero(),
        stop_reason: stop,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    }
}

/// Build an assistant message carrying tool-call blocks. Mirrors TS
/// `createAssistantMessage([...toolCalls], "toolUse")`.
pub fn assistant_tool_calls(
    calls: Vec<(&str, &str, serde_json::Value)>,
    stop: StopReason,
) -> AssistantMessage {
    let content: Vec<Content> = calls
        .into_iter()
        .map(|(id, name, args)| {
            Content::ToolCall(ToolCall {
                kind: ToolCallType,
                id: id.to_string(),
                name: name.to_string(),
                arguments: args,
                thought_signature: None,
                namespace: None,
                recorded: None,
            })
        })
        .collect();
    AssistantMessage {
        role: rpi_ai::types::AssistantRole,
        content,
        api: Api::Other("openai-responses".into()),
        provider: "mock".to_string(),
        model: "mock".to_string(),
        response_model: None,
        response_id: None,
        usage: rpi_ai::types::Usage::zero(),
        stop_reason: stop,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    }
}

/// The identity converter: keep only `user`/`assistant`/`toolResult` messages,
/// drop `custom`. Mirrors TS `identityConverter`. Returns the `ConvertToLlm`
/// Arc shape the loop config expects.
pub fn identity_converter() -> Arc<
    dyn Fn(Vec<rpi_agent::AgentMessage>) -> futures::future::BoxFuture<'static, Vec<Message>>
        + Send
        + Sync,
> {
    Arc::new(|messages: Vec<rpi_agent::AgentMessage>| {
        let out: Vec<Message> = messages
            .into_iter()
            .filter_map(|m| match m {
                rpi_agent::AgentMessage::User(u) => Some(Message::User(u)),
                rpi_agent::AgentMessage::Assistant(a) => Some(Message::Assistant(a)),
                rpi_agent::AgentMessage::ToolResult(t) => Some(Message::ToolResult(t)),
                rpi_agent::AgentMessage::Custom(_) => None,
            })
            .collect();
        Box::pin(async move { out })
    })
}

/// A minimal config with the identity converter + the mock model. Mirrors the
/// TS `{ model: createModel(), convertToLlm: identityConverter }` baseline.
pub fn base_config() -> rpi_agent::AgentLoopConfig {
    loop_config_with_converter(identity_converter())
}

/// Build a config with a specific converter + mock model.
pub fn loop_config_with_converter(
    convert_to_llm: rpi_agent::ConvertToLlm,
) -> rpi_agent::AgentLoopConfig {
    rpi_agent::AgentLoopConfig {
        model: mock_model(),
        convert_to_llm,
        convert_to_llm_incremental: None,
        transform_context: None,
        get_api_key: None,
        should_stop_after_turn: None,
        prepare_next_turn: None,
        after_tool_results: None,
        get_steering_messages: None,
        get_follow_up_messages: None,
        pending_message_count: None,
        before_tool_call: None,
        after_tool_call: None,
        tool_execution: rpi_agent::ToolExecutionMode::Parallel,
        thinking_level: rpi_ai::types::ThinkingLevel::Off,
        api_key: None,
        timeout: None,
        stream_idle_timeout: None,
        max_retries: None,
        max_retry_delay: None,
        cache_retention: rpi_ai::provider::CacheRetention::default(),
        session_id: None,
        signal: tokio_util::sync::CancellationToken::new(),
    }
}

/// Extract the text content of a `ToolResultMessage`.
pub fn tool_result_text(trm: &rpi_ai::types::ToolResultMessage) -> String {
    trm.content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Collect every event from a completed `run_agent_loop` into a `Vec`. Drains
/// the collector's buffer after the run settles.
pub async fn run_and_collect(
    prompts: Vec<rpi_agent::AgentMessage>,
    context: rpi_agent::AgentContext,
    config: rpi_agent::AgentLoopConfig,
    stream_fn: StreamFn,
) -> (Vec<rpi_agent::AgentEvent>, Vec<rpi_agent::AgentMessage>) {
    let (collector, events) = rpi_agent::CollectorEmitter::new();
    let emit: Arc<dyn rpi_agent::AgentEmitter> = Arc::new(collector);
    let new_messages = rpi_agent::run_agent_loop(prompts, context, config, emit, stream_fn)
        .await
        .expect("run_agent_loop failed");
    let events = events.lock().expect("events lock").clone();
    (events, new_messages)
}

/// Map an `AgentEvent` to its `type` tag — the Rust equivalent of the TS
/// `events.map((e) => e.type)`.
pub fn type_tags(events: &[rpi_agent::AgentEvent]) -> Vec<&'static str> {
    events.iter().map(|e| e.type_tag()).collect()
}

/// Pull the `tool_call_id`s of every `ToolExecutionEnd`, in emit order.
pub fn tool_execution_end_ids(events: &[rpi_agent::AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            rpi_agent::AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                Some(tool_call_id.clone())
            }
            _ => None,
        })
        .collect()
}

/// Pull the `tool_call_id`s of every `MessageEnd` whose payload is a
/// `toolResult` message, in emit (source/ordinal) order.
pub fn tool_result_message_ids(events: &[rpi_agent::AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            rpi_agent::AgentEvent::MessageEnd { message, .. } => match message {
                rpi_agent::AgentMessage::ToolResult(t) => Some(t.tool_call_id.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

// Suppress unused-import warnings for symbols re-exported only for test
// convenience; they're referenced indirectly through the helper fns above.
#[allow(unused_imports)]
use rpi_ai::types::Tool as _ToolUnused;

/// A canned tool-definition builder used by the test-doubled `AgentTool`
/// implementations: `Object { value: String }`. Mirrors the TS
/// `Type.Object({ value: Type.String() })`.
pub fn value_string_schema() -> Tool {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "value": { "type": "string" }
        },
        "required": ["value"],
        "additionalProperties": false,
    });
    Tool {
        name: "unused".to_string(),
        description: String::new(),
        parameters: rpi_ai::types::Schema::new(schema),
        constrained_sampling: None,
    }
}
