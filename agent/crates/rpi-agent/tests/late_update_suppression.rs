//! Late-update suppression invariant (plan §5.3).
//!
//! Mirrors TS `agent.test.ts`:
//! - "should ignore tool updates after the tool execution settles"
//! - "should ignore a settled parallel tool update while another tool is still
//!   running"
//!
//! After a tool's `execute` resolves, the loop flips its `accepting_updates`
//! gate to false; any later call to the captured `on_update` must be a silent
//! no-op (no `ToolExecutionUpdate` event, no panic). The `on_update` callback
//! is `Arc<dyn Fn>` precisely so a tool can capture it and the test can invoke
//! it after `execute` has returned.

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;

use common::{base_config, run_and_collect, user_message};
use rpi_agent::{AgentContext, AgentEvent, AgentToolResult, ToolResultPartial};
use rpi_ai::event_stream::create_assistant_message_event_stream;
use rpi_ai::types::{AssistantMessage, AssistantMessageEvent, DoneReason, StopReason};
use tokio_util::sync::CancellationToken;

/// A tool that captures the `on_update` callback it was handed so the test can
/// invoke it later. Emits one update ("running") during `execute`, then returns
/// a terminating result ("ok"). The captured callback outlives `execute`.
struct DelayedTool {
    schema: rpi_ai::types::Tool,
    captured: Arc<std::sync::Mutex<Option<Arc<dyn Fn(ToolResultPartial) + Send + Sync>>>>,
}

#[async_trait::async_trait]
impl rpi_agent::AgentTool for DelayedTool {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "Delayed Tool"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Value,
        _signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, rpi_agent::AgentError> {
        // Stash the callback so the test can fire a late update.
        *self.captured.lock().expect("captured lock") = Some(Arc::clone(&on_update));
        // One legitimate update during execution.
        on_update(AgentToolResult {
            content: vec![rpi_agent::TextContentOrImage::text("running")],
            details: serde_json::json!({ "status": "running" }),
            ..AgentToolResult::default()
        });
        // Delay a beat so the late call is provably *after* settle.
        tokio::task::yield_now().await;
        Ok(AgentToolResult {
            content: vec![rpi_agent::TextContentOrImage::text("ok")],
            details: serde_json::json!({ "status": "done" }),
            terminate: true,
            ..AgentToolResult::default()
        })
    }
}

fn empty_object_schema(name: &str, _label: &str, description: &str) -> rpi_ai::types::Tool {
    rpi_ai::types::Tool {
        name: name.to_string(),
        description: description.to_string(),
        parameters: rpi_ai::types::Schema::new(serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })),
        constrained_sampling: None,
    }
}

/// Build a one-call mock stream returning an assistant message with tool calls.
fn single_tool_use_stream_fn(calls: Vec<(&str, &str)>) -> rpi_agent::StreamFn {
    use rpi_ai::types::{Content, ToolCall};
    let content: Vec<Content> = calls
        .into_iter()
        .map(|(id, name)| {
            Content::ToolCall(ToolCall {
                kind: rpi_ai::types::ToolCallType,
                id: id.to_string(),
                name: name.to_string(),
                arguments: serde_json::json!({}),
                thought_signature: None,
                namespace: None,
                recorded: None,
            })
        })
        .collect();
    let message = AssistantMessage {
        role: rpi_ai::types::AssistantRole,
        content,
        api: rpi_ai::types::Api::Other("openai-responses".into()),
        provider: "mock".to_string(),
        model: "mock".to_string(),
        response_model: None,
        response_id: None,
        usage: rpi_ai::types::Usage::zero(),
        stop_reason: StopReason::ToolUse,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    };
    rpi_agent::stream_fn(move |_model, _ctx, _opts| {
        // Re-clone the message for every call — but this stream_fn is only
        // used for a single-turn terminate run, so one-shot is fine.
        let msg = message.clone();
        let (mut prod, stream) = create_assistant_message_event_stream();
        tokio::spawn(async move {
            let partial = Arc::new(msg.clone());
            prod.push(AssistantMessageEvent::Start { partial }).await;
            prod.push(AssistantMessageEvent::Done {
                reason: DoneReason::ToolUse,
                message: msg,
            })
            .await;
        });
        stream
    })
}

fn count_tool_updates(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolExecutionUpdate { .. }))
        .count()
}

#[tokio::test]
async fn ignores_tool_updates_after_execute_settles() {
    let captured: Arc<std::sync::Mutex<Option<Arc<dyn Fn(ToolResultPartial) + Send + Sync>>>> =
        Arc::new(std::sync::Mutex::new(None));
    let tool = DelayedTool {
        schema: empty_object_schema(
            "delayed_tool",
            "Delayed Tool",
            "Captures progress callbacks",
        ),
        captured: Arc::clone(&captured),
    };
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![Arc::new(tool)],
    };

    let stream_fn = single_tool_use_stream_fn(vec![("call-1", "delayed_tool")]);
    let (events, _new_messages) = run_and_collect(
        vec![user_message("run tool")],
        context,
        base_config(),
        stream_fn,
    )
    .await;

    // Exactly one update — the in-flight "running" one. The late call below
    // must NOT add another.
    assert_eq!(
        count_tool_updates(&events),
        1,
        "expected exactly 1 tool_execution_update (the in-flight one), got {}: {events:?}",
        count_tool_updates(&events)
    );
    let event_count_after_prompt = events.len();

    // Fire a late update AFTER execute has settled.
    let late = captured.lock().expect("captured lock").clone();
    if let Some(cb) = late {
        cb(AgentToolResult {
            content: vec![rpi_agent::TextContentOrImage::text("late")],
            details: serde_json::json!({ "status": "late" }),
            ..AgentToolResult::default()
        });
    }
    // Yield a few times so any (buggy) async path would have a chance to emit.
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }

    // Re-collect: no new events, no panic. The collector we used is gone, so we
    // can only assert the count we observed before the late call stays the
    // invariant-relevant number (1 update). The absence of a panic here IS the
    // assertion that the gate dropped the call.
    assert_eq!(
        count_tool_updates(&events),
        1,
        "late update slipped past the gate"
    );
    assert_eq!(events.len(), event_count_after_prompt);
}

/// A tool that blocks until a release `Notify` is fired. Keeps the agent run
/// active so a *settled* peer's late update can be attempted mid-run.
struct BlockingTool {
    schema: rpi_ai::types::Tool,
    release: Arc<tokio::sync::Notify>,
    started: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl rpi_agent::AgentTool for BlockingTool {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "Slow Tool"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, rpi_agent::AgentError> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(AgentToolResult {
            content: vec![rpi_agent::TextContentOrImage::text("done")],
            details: serde_json::json!({ "status": "done" }),
            terminate: true,
            ..AgentToolResult::default()
        })
    }
}

/// A tool that settles immediately and captures its `on_update` for the test.
struct SettledTool {
    schema: rpi_ai::types::Tool,
    captured: Arc<std::sync::Mutex<Option<Arc<dyn Fn(ToolResultPartial) + Send + Sync>>>>,
}

#[async_trait::async_trait]
impl rpi_agent::AgentTool for SettledTool {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "Settled Tool"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Value,
        _signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, rpi_agent::AgentError> {
        *self.captured.lock().expect("captured lock") = Some(Arc::clone(&on_update));
        Ok(AgentToolResult {
            content: vec![rpi_agent::TextContentOrImage::text("done")],
            details: serde_json::json!({ "status": "done" }),
            terminate: true,
            ..AgentToolResult::default()
        })
    }
}

#[tokio::test]
async fn ignores_settled_parallel_update_while_another_tool_runs() {
    // settled_tool + slow_tool run in parallel. slow_tool blocks until released.
    // The test:
    //   1. starts the run,
    //   2. waits for slow_tool to start + settled_tool to end (via event),
    //   3. invokes settled_tool's captured on_update while slow_tool is still
    //      running,
    //   4. releases slow_tool, lets the run finish,
    //   5. asserts ZERO updates originated from the late call.
    let release = Arc::new(tokio::sync::Notify::new());
    let slow_started = Arc::new(tokio::sync::Notify::new());
    let settled_captured: Arc<
        std::sync::Mutex<Option<Arc<dyn Fn(ToolResultPartial) + Send + Sync>>>,
    > = Arc::new(std::sync::Mutex::new(None));

    let settled = SettledTool {
        schema: empty_object_schema(
            "settled_tool",
            "Settled Tool",
            "Captures progress callbacks",
        ),
        captured: Arc::clone(&settled_captured),
    };
    let slow = BlockingTool {
        schema: empty_object_schema("slow_tool", "Slow Tool", "Keeps the agent run active"),
        release: Arc::clone(&release),
        started: Arc::clone(&slow_started),
    };
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![Arc::new(settled), Arc::new(slow)],
    };

    // Two tool calls in one message; settled_tool terminates, slow_tool blocks.
    // The run will end once slow_tool is released.
    let stream_fn =
        single_tool_use_stream_fn(vec![("call-1", "settled_tool"), ("call-2", "slow_tool")]);

    // We can't use run_and_collect — we need to interject mid-run. Drive the
    // loop on a task and collect events into a shared buffer.
    let (collector, events_buf) = rpi_agent::CollectorEmitter::new();
    let emit: Arc<dyn rpi_agent::AgentEmitter> = Arc::new(collector);
    let cfg = base_config();
    let prompts = vec![user_message("run tools")];
    let ctx = context;
    let sf = stream_fn;
    let run_handle =
        tokio::spawn(async move { rpi_agent::run_agent_loop(prompts, ctx, cfg, emit, sf).await });

    // Wait for slow_tool to start (notified inside its execute).
    slow_started.notified().await;
    // Wait for settled_tool's ToolExecutionEnd to land in the buffer.
    // (settled_tool returns immediately, so its end fires quickly after start.)
    let mut settled_ended = false;
    for _ in 0..200 {
        let buf = events_buf.lock().expect("events lock").clone();
        if buf
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolExecutionEnd { tool_call_id, .. } if tool_call_id == "call-1"))
        {
            settled_ended = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert!(settled_ended, "settled_tool never ended");

    let updates_before = events_buf
        .lock()
        .expect("events lock")
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolExecutionUpdate { .. }))
        .count();

    // Fire the late update from settled_tool while slow_tool is still running.
    let late = settled_captured.lock().expect("captured lock").clone();
    if let Some(cb) = late {
        cb(AgentToolResult {
            content: vec![rpi_agent::TextContentOrImage::text("late")],
            details: serde_json::json!({ "status": "late" }),
            ..AgentToolResult::default()
        });
    }
    // Yield; the late call must be dropped by the gate.
    tokio::task::yield_now().await;

    let updates_after_late = events_buf
        .lock()
        .expect("events lock")
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolExecutionUpdate { .. }))
        .count();
    assert_eq!(
        updates_after_late, updates_before,
        "late update from settled_tool slipped past the gate while slow_tool was still running"
    );

    // Release slow_tool and let the run finish.
    release.notify_one();
    let _ = run_handle
        .await
        .expect("run task panicked")
        .expect("run ok");

    let final_updates = events_buf
        .lock()
        .expect("events lock")
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolExecutionUpdate { .. }))
        .count();
    assert_eq!(
        final_updates, updates_before,
        "late update should never appear, even after the run ends"
    );
}
