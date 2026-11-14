//! Integration tests for the basic event-sequence contract.
//!
//! Mirrors the TS `agent-loop.test.ts` cases:
//! - "should emit events with AgentMessage types"
//! - "should stop after the current turn when shouldStopAfterTurn returns true"
//!   (the exact 12-event sequence)
//!
//! These exercise the happy-path turn shapes against a [`common::mock_stream_fn`]
//! that returns scripted [`AssistantMessage`]s.

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;

use common::{
    assistant_text, assistant_tool_calls, base_config, mock_stream_fn, run_and_collect, type_tags,
    user_message,
};
use rpi_agent::{AgentContext, AgentEvent};
use rpi_ai::types::StopReason;

#[tokio::test]
async fn loop_keeps_the_owned_history_payload_instead_of_copying_it() {
    let (echo, _) = EchoTool::new();
    let mut context = context_with_tools(vec![Arc::new(echo)]);
    let history = "owned history".repeat(4096);
    let original = history.as_ptr() as usize;
    context.messages.push(user_message(history));
    let mut config = base_config();
    config.should_stop_after_turn = Some(Arc::new(move |turn| {
        let rpi_agent::AgentMessage::User(user) = &turn.context.messages[0] else {
            panic!("history must remain first");
        };
        let rpi_ai::UserContent::Text(text) = &user.content else {
            panic!("expected text history");
        };
        assert_eq!(
            text.as_ptr() as usize,
            original,
            "an owned history needs no defensive copy"
        );
        Box::pin(async { true })
    }));
    let (_, messages) = run_and_collect(
        vec![user_message("echo something")],
        context,
        config,
        mock_stream_fn(vec![assistant_tool_calls(
            vec![("tool-1", "echo", serde_json::json!({"value":"hello"}))],
            StopReason::ToolUse,
        )]),
    )
    .await;
    assert_eq!(
        messages.len(),
        3,
        "returned messages contain only this run's additions"
    );
}

/// A scripted echo tool — mirrors the TS `echo` tool used across
/// `agent-loop.test.ts`. Records every `value` it executed so tests can assert
/// ordering + that (un)expected calls did/didn't run.
struct EchoTool {
    schema: rpi_ai::types::Tool,
    executed: Arc<std::sync::Mutex<Vec<String>>>,
}

impl EchoTool {
    fn new() -> (Self, Arc<std::sync::Mutex<Vec<String>>>) {
        let executed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let schema = rpi_ai::types::Tool {
            name: "echo".to_string(),
            description: "Echo tool".to_string(),
            parameters: rpi_ai::types::Schema::new(serde_json::json!({
                "type": "object",
                "properties": { "value": { "type": "string" } },
                "required": ["value"],
                "additionalProperties": false,
            })),
            constrained_sampling: None,
        };
        let tool = EchoTool {
            schema,
            executed: Arc::clone(&executed),
        };
        (tool, executed)
    }
}

#[async_trait::async_trait]
impl rpi_agent::AgentTool for EchoTool {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "Echo"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: tokio_util::sync::CancellationToken,
        _on_update: Arc<dyn Fn(rpi_agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<rpi_agent::AgentToolResult, rpi_agent::AgentError> {
        let value = params
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        self.executed
            .lock()
            .expect("executed lock")
            .push(value.clone());
        Ok(rpi_agent::AgentToolResult::text(format!("echoed: {value}")))
    }
}

/// Build an `AgentContext` with the given tools + an empty prompt history.
fn context_with_tools(tools: Vec<Arc<dyn rpi_agent::AgentTool>>) -> AgentContext {
    AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools,
    }
}

#[tokio::test]
async fn emits_event_types_for_text_turn() {
    // Mirrors TS "should emit events with AgentMessage types".
    let stream_fn = mock_stream_fn(vec![assistant_text("Hi there!", StopReason::Stop)]);
    let (events, new_messages) = run_and_collect(
        vec![user_message("Hello")],
        AgentContext::default(),
        base_config(),
        stream_fn,
    )
    .await;

    // 2 messages: the user prompt + the assistant reply.
    assert_eq!(new_messages.len(), 2);
    assert_eq!(new_messages[0].role().as_str(), "user");
    assert_eq!(new_messages[1].role().as_str(), "assistant");

    // The required event types are present.
    let tags = type_tags(&events);
    assert!(
        tags.contains(&"agent_start"),
        "missing agent_start: {tags:?}"
    );
    assert!(tags.contains(&"turn_start"), "missing turn_start: {tags:?}");
    assert!(
        tags.contains(&"message_start"),
        "missing message_start: {tags:?}"
    );
    assert!(
        tags.contains(&"message_end"),
        "missing message_end: {tags:?}"
    );
    assert!(tags.contains(&"turn_end"), "missing turn_end: {tags:?}");
    assert!(tags.contains(&"agent_end"), "missing agent_end: {tags:?}");
}

#[tokio::test]
async fn emits_exact_sequence_when_should_stop_after_turn() {
    // Mirrors TS "should stop after the current turn when shouldStopAfterTurn
    // returns true". The exact 12-event sequence:
    //   agent_start, turn_start,
    //   message_start, message_end,        (user prompt)
    //   message_start, message_end,        (assistant tool-call message)
    //   tool_execution_start, tool_execution_end,
    //   message_start, message_end,        (tool result)
    //   turn_end, agent_end.
    let (echo, executed) = EchoTool::new();
    let context = context_with_tools(vec![Arc::new(echo)]);

    // `ShouldStopAfterTurn` takes a single `ShouldStopAfterTurnContext` arg
    // (no cancellation token — unlike before/afterToolCall). Returns `true`
    // so the loop stops after exactly one turn.
    let stop_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let should_stop: rpi_agent::ShouldStopAfterTurn = {
        let stop_calls = Arc::clone(&stop_calls);
        Arc::new(move |_ctx: rpi_agent::ShouldStopAfterTurnContext<'_>| {
            let stop_calls = Arc::clone(&stop_calls);
            Box::pin(async move {
                stop_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                true
            })
        })
    };

    let mut config = base_config();
    config.should_stop_after_turn = Some(should_stop);

    let stream_fn = mock_stream_fn(vec![assistant_tool_calls(
        vec![("tool-1", "echo", serde_json::json!({ "value": "hello" }))],
        StopReason::ToolUse,
    )]);
    let (events, new_messages) = run_and_collect(
        vec![user_message("echo something")],
        context,
        config,
        stream_fn,
    )
    .await;

    assert_eq!(
        new_messages
            .iter()
            .map(|m| m.role().as_str().to_string())
            .collect::<Vec<_>>(),
        vec![
            "user".to_string(),
            "assistant".to_string(),
            "toolResult".to_string()
        ],
    );
    assert_eq!(
        executed.lock().expect("executed lock").as_slice(),
        &["hello"]
    );
    assert_eq!(stop_calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    assert_eq!(
        type_tags(&events),
        vec![
            "agent_start",
            "turn_start",
            "message_start",
            "message_end",
            "message_start",
            "message_end",
            "tool_execution_start",
            "tool_execution_end",
            "message_start",
            "message_end",
            "turn_end",
            "agent_end",
        ]
    );
}

#[tokio::test]
async fn emits_message_update_events_on_streaming_deltas() {
    // A faux-like stream emitting a Start + TextStart + TextDelta + TextEnd +
    // Done should produce message_update events between the start and end.
    use rpi_ai::event_stream::create_assistant_message_event_stream;
    use rpi_ai::types::{AssistantMessage, AssistantMessageEvent, DoneReason, StopReason};

    // Build a custom stream_fn that emits a richer event sequence than the
    // common mock (which only pushes Start + terminal).
    let stream_fn = rpi_agent::stream_fn(move |_model, _ctx, _opts| {
        let (mut prod, stream) = create_assistant_message_event_stream();
        tokio::spawn(async move {
            let mut partial = AssistantMessage::empty(
                rpi_ai::types::Api::Other("openai-responses".into()),
                "mock",
                "mock",
                0,
            );
            partial.content.push(rpi_ai::types::Content::text(""));
            let p = std::sync::Arc::new(partial.clone());
            prod.push(AssistantMessageEvent::Start { partial: p.clone() })
                .await;
            prod.push(AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: p.clone(),
            })
            .await;
            prod.push(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "Hi ".into(),
                partial: p.clone(),
            })
            .await;
            prod.push(AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "there!".into(),
                partial: p.clone(),
            })
            .await;
            prod.push(AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: "Hi there!".into(),
                partial: p.clone(),
            })
            .await;
            let mut final_msg = (*p).clone();
            final_msg.stop_reason = StopReason::Stop;
            final_msg.content.clear();
            final_msg
                .content
                .push(rpi_ai::types::Content::text("Hi there!"));
            prod.push(AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message: final_msg,
            })
            .await;
        });
        stream
    });

    let (events, _new_messages) = run_and_collect(
        vec![user_message("Hello")],
        AgentContext::default(),
        base_config(),
        stream_fn,
    )
    .await;

    let update_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::MessageUpdate { .. }))
        .count();
    assert!(
        update_count >= 4,
        "expected at least 4 message_update events (text_start + 2 deltas + text_end), got {update_count}"
    );
}

/// `MessageUpdate` carries the streaming partial as a *shared* snapshot, so the
/// loop must forward the provider's `Arc` rather than deep-cloning the whole
/// growing message once per delta. That clone (plus one in the loop's context
/// update, plus one per consumer) is what made a long stream quadratic in the
/// output length — see `docs/debugging/llm-repetition-forensics.md` §十.
///
/// Pointer identity is the precise assertion: it can only hold if no copy
/// happened anywhere between the provider and the event.
#[tokio::test]
async fn message_update_forwards_the_provider_snapshot_without_copying() {
    use rpi_ai::event_stream::create_assistant_message_event_stream;
    use rpi_ai::types::{AssistantMessage, AssistantMessageEvent, DoneReason};

    let stream_fn = rpi_agent::stream_fn(move |_model, _ctx, _opts| {
        let (mut prod, stream) = create_assistant_message_event_stream();
        tokio::spawn(async move {
            let mut partial = AssistantMessage::empty(
                rpi_ai::types::Api::Other("openai-responses".into()),
                "mock",
                "mock",
                0,
            );
            partial.content.push(rpi_ai::types::Content::text(""));
            // One allocation reused for every event, exactly like a provider
            // that builds the snapshot once per emitted (batched) delta.
            let snapshot = Arc::new(partial.clone());
            prod.push(AssistantMessageEvent::Start {
                partial: Arc::clone(&snapshot),
            })
            .await;
            for delta in ["Hi ", "there!"] {
                prod.push(AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: delta.into(),
                    partial: Arc::clone(&snapshot),
                })
                .await;
            }
            let mut final_msg = (*snapshot).clone();
            final_msg.stop_reason = StopReason::Stop;
            final_msg.content.clear();
            final_msg
                .content
                .push(rpi_ai::types::Content::text("Hi there!"));
            prod.push(AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message: final_msg,
            })
            .await;
        });
        stream
    });

    let (events, _new_messages) = run_and_collect(
        vec![user_message("Hello")],
        AgentContext::default(),
        base_config(),
        stream_fn,
    )
    .await;

    let mut checked = 0usize;
    for event in &events {
        let AgentEvent::MessageUpdate {
            message,
            assistant_message_event,
            message_id,
        } = event
        else {
            continue;
        };
        assert!(
            message_id.is_none(),
            "direct agents have no session-owned identity"
        );
        let partial = match assistant_message_event {
            AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ToolCallDelta { partial, .. } => partial,
            _ => continue,
        };
        assert!(
            Arc::ptr_eq(message, partial),
            "MessageUpdate must forward the provider's snapshot, not copy it"
        );
        checked += 1;
    }
    assert!(
        checked >= 2,
        "expected at least the two text deltas to be checked, got {checked}"
    );
}
