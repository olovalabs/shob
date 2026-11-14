//! Abort / cancellation (plan §2.3 / `abort.rs`).
//!
//! Mirrors TS `agent.test.ts`:
//! - "should pass the active abort signal to subscribers"
//! - "should handle abort controller"
//!
//! The TS `AbortSignal` is `tokio_util::sync::CancellationToken` in Rust. The
//! loop threads `config.signal` into every stream_fn invocation (`SimpleStreamOptions`)
//! and into each tool's `execute` (as a child token), so a single `cancel()` on
//! the run token propagates everywhere. These tests cover:
//!
//! 1. **abort-during-stream**: a stream_fn that polls the token and emits an
//!    `Error { Aborted }` event on cancel produces an `AgentEnd` (no panic, no
//!    leaked task) — the TS "active abort signal" behavior.
//! 2. **abort-during-tool**: a tool that blocks on its child token being
//!    cancelled resolves cleanly when the run token is cancelled; the run
//!    settles to `AgentEnd` and the blocking task does not leak.
//! 3. **abort-idempotent-no-run**: cancelling a token that was never attached to
//!    a run (the TS "should handle abort controller" `expect(() =>
//!    agent.abort()).not.toThrow()` shape) is a plain no-op.

#[path = "common/mod.rs"]
mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use common::{assistant_tool_calls, base_config};
use rpi_agent::{AbortHandle, AgentContext, AgentEvent, AgentToolResult};
use rpi_ai::event_stream::create_assistant_message_event_stream;
use rpi_ai::types::{AssistantMessage, AssistantMessageEvent, ErrorReason, StopReason};
use tokio_util::sync::CancellationToken;

/// True once an `AgentEnd` event has appeared.
fn saw_agent_end(events: &[AgentEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, AgentEvent::AgentEnd { .. }))
}

// ----------------------------------------------------------------------------
// abort-during-stream: the stream_fn polls the token and emits Error { Aborted }
// ----------------------------------------------------------------------------

#[tokio::test]
async fn abort_during_stream_produces_agent_end() {
    // Mirrors TS "should pass the active abort signal to subscribers". The
    // stream_fn pushes Start, then polls `options.signal` every 2ms; on cancel
    // it pushes an Error event carrying an Aborted stop reason. The loop must
    // fold that into a MessageEnd + TurnEnd + AgentEnd and return.
    let handle = AbortHandle::new();
    let token = handle.token();

    let stream_fn = rpi_agent::stream_fn(move |_model, _ctx, opts| {
        let token = opts.signal.clone();
        let (mut prod, stream) = create_assistant_message_event_stream();
        tokio::spawn(async move {
            let partial = AssistantMessage::empty(
                rpi_ai::types::Api::Other("openai-responses".into()),
                "mock",
                "mock",
                0,
            );
            prod.push(AssistantMessageEvent::Start {
                partial: Arc::new(partial),
            })
            .await;
            // Poll the token until cancelled (TS `checkAbort` loop).
            loop {
                if token.is_cancelled() {
                    let aborted = AssistantMessage::terminal(
                        rpi_ai::types::Api::Other("mock".into()),
                        "mock",
                        "mock",
                        StopReason::Aborted,
                        "Aborted",
                        0,
                    );
                    prod.push(AssistantMessageEvent::Error {
                        reason: ErrorReason::Aborted,
                        error: aborted,
                    })
                    .await;
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        });
        stream
    });

    // Wire the run token into the config (the loop forwards it to stream_opts).
    let mut config = base_config();
    config.signal = token.clone();

    let (collector, events_buf) = rpi_agent::CollectorEmitter::new();
    let emit: Arc<dyn rpi_agent::AgentEmitter> = Arc::new(collector);

    // Drive the run on a task so we can cancel mid-stream.
    let run_handle = tokio::spawn(async move {
        rpi_agent::run_agent_loop(
            vec![common::user_message("hello")],
            AgentContext::default(),
            config,
            emit,
            stream_fn,
        )
        .await
    });

    // Give the stream a moment to start, then abort.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    handle.abort();

    let new_messages = run_handle
        .await
        .expect("run task did not panic")
        .expect("run resolves Ok on abort-with-Error-event");
    let events = events_buf.lock().expect("events lock").clone();

    // An AgentEnd was emitted — no hang, no leaked task.
    assert!(
        saw_agent_end(&events),
        "abort during stream must emit AgentEnd, got: {events:?}"
    );
    // The aborted assistant message is in the new messages.
    let last = new_messages.last().expect("non-empty new messages");
    assert_eq!(last.role().as_str(), "assistant");
}

// ----------------------------------------------------------------------------
// abort-during-tool: a blocking tool unblocks on cancellation
// ----------------------------------------------------------------------------

/// A tool that blocks until its (child) cancellation token fires, then returns
/// a normal result. If the run token is cancelled, the child is too, so the
/// tool resolves and the run settles.
struct BlockingTool {
    schema: rpi_ai::types::Tool,
    started: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl rpi_agent::AgentTool for BlockingTool {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "Blocking"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(rpi_agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, rpi_agent::AgentError> {
        self.started.notify_one();
        // Block until cancelled. `cancelled()` resolves when the token fires.
        signal.cancelled().await;
        Ok(AgentToolResult::text("unblocked"))
    }
}

fn empty_schema(name: &str) -> rpi_ai::types::Tool {
    rpi_ai::types::Tool {
        name: name.to_string(),
        description: "Blocking tool".to_string(),
        parameters: rpi_ai::types::Schema::new(serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })),
        constrained_sampling: None,
    }
}

#[tokio::test]
async fn abort_during_tool_unblocks_and_settles() {
    // A blocking tool waits on its child token. The run token is cancelled
    // mid-tool; the child fires, the tool resolves, and the run settles to
    // AgentEnd without a leaked task.
    let started = Arc::new(tokio::sync::Notify::new());
    let tool = BlockingTool {
        schema: empty_schema("blocking"),
        started: Arc::clone(&started),
    };
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![Arc::new(tool)],
    };

    let handle = AbortHandle::new();
    let token = handle.token();

    let mut config = base_config();
    config.signal = token.clone();

    // A steering message entered while the blocking tool was active must be
    // drained after cancellation so it is retained in the returned context,
    // matching upstream's post-tool drain ordering.
    let delivered = Arc::new(AtomicBool::new(false));
    let delivered_for_hook = Arc::clone(&delivered);
    let token_for_hook = token.clone();
    config.get_steering_messages = Some(Arc::new(move || {
        let delivered = Arc::clone(&delivered_for_hook);
        let token = token_for_hook.clone();
        Box::pin(async move {
            if token.is_cancelled() && !delivered.swap(true, Ordering::SeqCst) {
                vec![common::user_message("queued while listening")]
            } else {
                Vec::new()
            }
        })
    }));

    let stream_fn = common::mock_stream_fn(vec![assistant_tool_calls(
        vec![("tool-1", "blocking", serde_json::json!({}))],
        StopReason::ToolUse,
    )]);

    let (collector, events_buf) = rpi_agent::CollectorEmitter::new();
    let emit: Arc<dyn rpi_agent::AgentEmitter> = Arc::new(collector);
    let run_handle = tokio::spawn(async move {
        rpi_agent::run_agent_loop(
            vec![common::user_message("run the tool")],
            context,
            config,
            emit,
            stream_fn,
        )
        .await
    });

    // Wait until the tool has actually started executing, then abort.
    started.notified().await;
    handle.abort();

    let new_messages = run_handle.await.expect("run task did not panic");
    let new_messages = new_messages.expect("run resolves cleanly on tool abort");
    let events = events_buf.lock().expect("events lock").clone();
    assert!(
        saw_agent_end(&events),
        "abort during tool must still emit AgentEnd: {events:?}"
    );
    assert!(
        new_messages.iter().any(|message| {
            matches!(message, rpi_agent::AgentMessage::User(user)
                if user.content.as_text() == Some("queued while listening"))
        }),
        "queued steering must be included in the aborted run transcript"
    );
}

// ----------------------------------------------------------------------------
// abort with no run is a no-op (TS "should handle abort controller")
// ----------------------------------------------------------------------------

#[tokio::test]
async fn abort_with_no_active_run_is_a_no_op() {
    // Mirrors TS `expect(() => agent.abort()).not.toThrow()`. Cancelling a
    // token that isn't attached to any run must not panic and is idempotent.
    let handle = AbortHandle::new();
    // Not a run — just call abort on a fresh handle.
    handle.abort();
    assert!(handle.is_aborted());
    // Idempotent: a second abort is a no-op (no panic).
    handle.abort();
    assert!(handle.is_aborted());

    // A child token of an already-aborted handle is born cancelled, but this
    // is still safe to obtain and inspect.
    let child = handle.child();
    let saw = Arc::new(AtomicBool::new(false));
    let saw2 = Arc::clone(&saw);
    // `cancelled().await` resolves immediately on a born-cancelled token.
    let t = tokio::spawn(async move {
        child.cancelled().await;
        saw2.store(true, Ordering::SeqCst);
    });
    t.await.expect("child task did not panic");
    assert!(
        saw.load(Ordering::SeqCst),
        "born-cancelled child should resolve cancelled()"
    );
}
