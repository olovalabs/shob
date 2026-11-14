//! Regression: a post-tool abort must be visible to message-only callers and persistence.
#[path = "common/mod.rs"]
mod common;

use rpi_agent::{AgentContext, AgentEvent, AgentMessage, AgentTool, AgentToolResult};
use rpi_ai::types::{StopReason, Tool};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use tokio_util::sync::CancellationToken;

struct PendingTool {
    schema: Tool,
    started: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl AgentTool for PendingTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "pending"
    }
    async fn execute(
        &self,
        _: &str,
        _: serde_json::Value,
        signal: CancellationToken,
        _: Arc<dyn Fn(rpi_agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, rpi_agent::AgentError> {
        self.started.notify_one();
        signal.cancelled().await;
        Ok(AgentToolResult::text("tool settled"))
    }
}

async fn post_tool_abort_case(resume: bool, steering: bool) {
    let token = CancellationToken::new();
    let started = Arc::new(tokio::sync::Notify::new());
    let assistant = common::assistant_tool_calls(
        vec![("call-1", "pending", serde_json::json!({}))],
        StopReason::ToolUse,
    );
    let original = AgentMessage::Assistant(Box::new(assistant.clone()));
    let tool = PendingTool {
        schema: Tool {
            name: "pending".into(),
            description: "pending".into(),
            parameters: rpi_ai::types::Schema::new(serde_json::json!({"type":"object"})),
            constrained_sampling: None,
        },
        started: started.clone(),
    };
    let context = AgentContext {
        messages: if resume {
            vec![common::user_message("run"), original.clone()]
        } else {
            vec![]
        },
        tools: vec![Arc::new(tool)],
        ..Default::default()
    };
    let mut config = common::base_config();
    config.signal = token.clone();
    let delivered = Arc::new(AtomicBool::new(false));
    if steering {
        let token = token.clone();
        config.get_steering_messages = Some(Arc::new(move || {
            let token = token.clone();
            let delivered = delivered.clone();
            Box::pin(async move {
                if token.is_cancelled() && !delivered.swap(true, Ordering::SeqCst) {
                    vec![common::user_message("queued steering")]
                } else {
                    vec![]
                }
            })
        }));
    }
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let calls = provider_calls.clone();
    let scripted = common::mock_stream_fn(vec![assistant]);
    let stream = rpi_agent::stream_fn(move |model, context, opts| {
        calls.fetch_add(1, Ordering::SeqCst);
        scripted(model, context, opts)
    });
    let (collector, events) = rpi_agent::CollectorEmitter::new();
    let emit: Arc<dyn rpi_agent::AgentEmitter> = Arc::new(collector);
    let run = tokio::spawn(async move {
        if resume {
            rpi_agent::run_agent_loop_from_assistant(context, config, emit, stream).await
        } else {
            rpi_agent::run_agent_loop(
                vec![common::user_message("run")],
                context,
                config,
                emit,
                stream,
            )
            .await
        }
    });
    started.notified().await;
    token.cancel();
    let messages = tokio::time::timeout(std::time::Duration::from_secs(3), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        provider_calls.load(Ordering::SeqCst),
        usize::from(!resume),
        "no provider call to fabricate abort"
    );
    let AgentMessage::Assistant(terminal) = messages.last().unwrap() else {
        panic!("missing terminal assistant")
    };
    assert_eq!(terminal.stop_reason, StopReason::Aborted);
    assert!(terminal.content.is_empty());
    assert!(terminal.error_message.is_some());
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m, AgentMessage::ToolResult(_)))
            .count(),
        1
    );
    if !resume {
        assert_eq!(
            messages[1], original,
            "do not mutate the tool-call assistant"
        );
    }
    assert_eq!(
        messages.iter().any(
            |m| matches!(m, AgentMessage::User(u) if u.content.as_text() == Some("queued steering"))
        ),
        steering
    );
    let events = events.lock().unwrap();
    let aborted = messages.last().unwrap();
    let result_end = events
        .iter()
        .position(|e| {
            matches!(
                e,
                AgentEvent::MessageEnd {
                    message: AgentMessage::ToolResult(_),
                    ..
                }
            )
        })
        .unwrap();
    let abort_start = events
        .iter()
        .position(|e| matches!(e, AgentEvent::MessageStart { message, .. } if message == aborted))
        .unwrap();
    assert!(result_end < abort_start);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::MessageEnd { message, .. } if message == aborted))
            .count(),
        1
    );
    assert!(
        matches!(&events[events.len()-2], AgentEvent::TurnEnd { message, tool_results, .. } if message == aborted && tool_results.is_empty())
    );
    assert!(
        matches!(events.last(), Some(AgentEvent::AgentEnd { messages: ended }) if ended == &messages)
    );
}

#[tokio::test]
async fn post_tool_abort_inflight_without_steering() {
    post_tool_abort_case(false, false).await;
}
#[tokio::test]
async fn post_tool_abort_inflight_with_steering() {
    post_tool_abort_case(false, true).await;
}
#[tokio::test]
async fn post_tool_abort_resume_without_steering() {
    post_tool_abort_case(true, false).await;
}
#[tokio::test]
async fn post_tool_abort_resume_with_steering() {
    post_tool_abort_case(true, true).await;
}

#[tokio::test]
async fn post_tool_late_cancel_does_not_relabel_normal_completed_assistant() {
    let mut config = common::base_config();
    let token = config.signal.clone();
    config.should_stop_after_turn = Some(Arc::new(move |_| {
        token.cancel();
        Box::pin(async { false })
    }));
    let (events, messages) = common::run_and_collect(
        vec![common::user_message("hello")],
        AgentContext::default(),
        config,
        common::mock_stream_fn(vec![common::assistant_text("done", StopReason::Stop)]),
    )
    .await;
    assert!(
        matches!(messages.last(), Some(AgentMessage::Assistant(a)) if a.stop_reason == StopReason::Stop)
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(
                e,
                AgentEvent::MessageEnd {
                    message: AgentMessage::Assistant(_),
                    ..
                }
            ))
            .count(),
        1
    );
}
