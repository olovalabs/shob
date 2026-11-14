//! Regressions for provider-message cache invalidation around injected messages.
//!
//! The full converter below deliberately removes orphaned tool results, which
//! is the repair behavior a harness converter provides. The incremental
//! converter deliberately does not repair them, making an accidental cache hit
//! visible in the provider-facing context.

#[path = "common/mod.rs"]
mod common;

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::{assistant_text, assistant_tool_calls, base_config, user_message};
use rpi_agent::{
    AgentContext, AgentMessage, AgentToolResult, ConvertToLlm, GetFollowUpMessages,
    GetSteeringMessages, IncrementalConvertToLlm,
};
use rpi_ai::types::{Content, Message, StopReason, Tool, ToolResultMessage, ToolResultRole};
use tokio_util::sync::CancellationToken;

fn repairing_converter(calls: Arc<AtomicUsize>) -> ConvertToLlm {
    Arc::new(move |messages| {
        calls.fetch_add(1, Ordering::SeqCst);
        let mut known_tool_calls = HashSet::new();
        let converted = messages
            .into_iter()
            .filter_map(|message| match message {
                AgentMessage::User(user) => Some(Message::User(user)),
                AgentMessage::Assistant(assistant) => {
                    for block in &assistant.content {
                        if let Content::ToolCall(call) = block {
                            known_tool_calls.insert(call.id.clone());
                        }
                    }
                    Some(Message::Assistant(assistant))
                }
                AgentMessage::ToolResult(result) => known_tool_calls
                    .contains(&result.tool_call_id)
                    .then_some(Message::ToolResult(result)),
                AgentMessage::Custom(_) => None,
            })
            .collect::<Vec<_>>();
        Box::pin(async move { converted })
    })
}

fn permissive_incremental(calls: Arc<AtomicUsize>) -> IncrementalConvertToLlm {
    Arc::new(move |messages| {
        calls.fetch_add(1, Ordering::SeqCst);
        messages
            .iter()
            .filter_map(|message| match message {
                AgentMessage::User(user) => Some(Message::User(user.clone())),
                AgentMessage::Assistant(assistant) => Some(Message::Assistant(assistant.clone())),
                AgentMessage::ToolResult(result) => Some(Message::ToolResult(result.clone())),
                AgentMessage::Custom(_) => None,
            })
            .collect()
    })
}

fn late_tool_result() -> AgentMessage {
    AgentMessage::ToolResult(Box::new(ToolResultMessage {
        role: ToolResultRole,
        tool_call_id: "orphan-late-result".into(),
        tool_name: "echo".into(),
        content: vec![Content::text("late")],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        is_error: false,
        timestamp: 0,
    }))
}

fn echo_tool() -> Arc<dyn rpi_agent::AgentTool> {
    struct Echo;

    #[async_trait::async_trait]
    impl rpi_agent::AgentTool for Echo {
        fn schema(&self) -> &Tool {
            static SCHEMA: std::sync::OnceLock<Tool> = std::sync::OnceLock::new();
            SCHEMA.get_or_init(|| Tool {
                name: "echo".into(),
                description: "echo".into(),
                parameters: rpi_ai::types::Schema::new(serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                })),
                constrained_sampling: None,
            })
        }

        fn label(&self) -> &str {
            "Echo"
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _params: serde_json::Value,
            _signal: CancellationToken,
            _on_update: Arc<dyn Fn(rpi_agent::ToolResultPartial) + Send + Sync>,
        ) -> Result<AgentToolResult, rpi_agent::AgentError> {
            Ok(AgentToolResult::text("ok"))
        }
    }

    Arc::new(Echo)
}

fn context_recording_stream(
    inner: rpi_agent::StreamFn,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
) -> rpi_agent::StreamFn {
    rpi_agent::stream_fn(move |model, context, options| {
        seen.lock()
            .expect("context lock")
            .push(context.messages.clone());
        inner(model, context, options)
    })
}

fn context_lengths(seen: &Arc<Mutex<Vec<Vec<Message>>>>) -> Vec<usize> {
    seen.lock()
        .expect("context lock")
        .iter()
        .map(Vec::len)
        .collect()
}

#[tokio::test]
async fn steering_tool_result_injection_uses_full_repairing_converter() {
    let full_calls = Arc::new(AtomicUsize::new(0));
    let incremental_calls = Arc::new(AtomicUsize::new(0));
    let mut config = base_config();
    config.convert_to_llm = repairing_converter(Arc::clone(&full_calls));
    config.convert_to_llm_incremental =
        Some(permissive_incremental(Arc::clone(&incremental_calls)));
    config.get_steering_messages = Some({
        let polls = Arc::new(AtomicUsize::new(0));
        Arc::new(
            move || -> futures::future::BoxFuture<'static, Vec<AgentMessage>> {
                let polls = Arc::clone(&polls);
                Box::pin(async move {
                    (polls.fetch_add(1, Ordering::SeqCst) == 1)
                        .then(late_tool_result)
                        .into_iter()
                        .collect()
                })
            },
        ) as GetSteeringMessages
    });

    let seen = Arc::new(Mutex::new(Vec::new()));
    let stream = context_recording_stream(
        common::mock_stream_fn(vec![
            assistant_tool_calls(
                vec![("call-1", "echo", serde_json::json!({}))],
                StopReason::ToolUse,
            ),
            assistant_text("done", StopReason::Stop),
        ]),
        Arc::clone(&seen),
    );
    let context = AgentContext {
        tools: vec![echo_tool()],
        ..AgentContext::default()
    };

    rpi_agent::run_agent_loop(
        vec![user_message("start")],
        context,
        config,
        Arc::new(rpi_agent::CollectorEmitter::default()),
        stream,
    )
    .await
    .expect("steering run succeeds");

    assert_eq!(context_lengths(&seen), vec![1, 3]);
    assert_eq!(full_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        incremental_calls.load(Ordering::SeqCst),
        0,
        "an injected steering message must not use the suffix converter"
    );
    assert!(!seen.lock().unwrap()[1].iter().any(|message| {
        matches!(message, Message::ToolResult(result) if result.tool_call_id == "orphan-late-result")
    }));
}

#[tokio::test]
async fn follow_up_tool_result_injection_uses_full_repairing_converter() {
    let full_calls = Arc::new(AtomicUsize::new(0));
    let incremental_calls = Arc::new(AtomicUsize::new(0));
    let mut config = base_config();
    config.convert_to_llm = repairing_converter(Arc::clone(&full_calls));
    config.convert_to_llm_incremental =
        Some(permissive_incremental(Arc::clone(&incremental_calls)));
    config.get_follow_up_messages = Some({
        let polls = Arc::new(AtomicUsize::new(0));
        Arc::new(
            move || -> futures::future::BoxFuture<'static, Vec<AgentMessage>> {
                let polls = Arc::clone(&polls);
                Box::pin(async move {
                    (polls.fetch_add(1, Ordering::SeqCst) == 0)
                        .then(late_tool_result)
                        .into_iter()
                        .collect()
                })
            },
        ) as GetFollowUpMessages
    });

    let seen = Arc::new(Mutex::new(Vec::new()));
    let stream = context_recording_stream(
        common::mock_stream_fn(vec![
            assistant_text("first", StopReason::Stop),
            assistant_text("second", StopReason::Stop),
        ]),
        Arc::clone(&seen),
    );
    rpi_agent::run_agent_loop(
        vec![user_message("start")],
        AgentContext::default(),
        config,
        Arc::new(rpi_agent::CollectorEmitter::default()),
        stream,
    )
    .await
    .expect("follow-up run succeeds");

    assert_eq!(context_lengths(&seen), vec![1, 2]);
    assert_eq!(full_calls.load(Ordering::SeqCst), 2);
    assert_eq!(incremental_calls.load(Ordering::SeqCst), 0);
    assert!(!seen.lock().unwrap()[1].iter().any(|message| {
        matches!(message, Message::ToolResult(result) if result.tool_call_id == "orphan-late-result")
    }));
}

#[tokio::test]
async fn ordinary_tool_round_still_uses_incremental_conversion() {
    let full_calls = Arc::new(AtomicUsize::new(0));
    let incremental_calls = Arc::new(AtomicUsize::new(0));
    let mut config = base_config();
    config.convert_to_llm = repairing_converter(Arc::clone(&full_calls));
    config.convert_to_llm_incremental =
        Some(permissive_incremental(Arc::clone(&incremental_calls)));

    let seen = Arc::new(Mutex::new(Vec::new()));
    let stream = context_recording_stream(
        common::mock_stream_fn(vec![
            assistant_tool_calls(
                vec![("call-1", "echo", serde_json::json!({}))],
                StopReason::ToolUse,
            ),
            assistant_text("done", StopReason::Stop),
        ]),
        Arc::clone(&seen),
    );
    let context = AgentContext {
        tools: vec![echo_tool()],
        ..AgentContext::default()
    };

    rpi_agent::run_agent_loop(
        vec![user_message("start")],
        context,
        config,
        Arc::new(rpi_agent::CollectorEmitter::default()),
        stream,
    )
    .await
    .expect("ordinary tool run succeeds");

    assert_eq!(context_lengths(&seen), vec![1, 3]);
    assert_eq!(full_calls.load(Ordering::SeqCst), 1);
    assert_eq!(incremental_calls.load(Ordering::SeqCst), 1);
    assert!(seen.lock().unwrap()[1].iter().any(|message| {
        matches!(message, Message::ToolResult(result) if result.tool_call_id == "call-1")
    }));
}
