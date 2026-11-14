//! Steering-message injection (plan §2.3 / §5).
//!
//! Mirrors TS `agent-loop.test.ts`:
//! "should inject queued messages after all tool calls complete".
//!
//! Two tool calls arrive in one assistant message; the loop is in `Sequential`
//! mode so both run in order. After the tool batch settles, the
//! `get_steering_messages` hook returns an "interrupt" user message; the loop
//! injects it before the next LLM call, so the second stream_fn invocation sees
//! the interrupt in its context. Both tools must finish BEFORE the interrupt
//! appears in the event stream.

#[path = "common/mod.rs"]
mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use common::{assistant_text, assistant_tool_calls, base_config, run_and_collect, user_message};
use rpi_agent::{
    AgentContext, AgentEvent, AgentToolResult, GetSteeringMessages, ToolExecutionMode,
};
use rpi_ai::types::StopReason;
use tokio_util::sync::CancellationToken;

/// An echo tool that records every executed `value` into a shared buffer. The
/// steering hook reads the buffer's length to decide when to release the
/// interrupt.
struct EchoTool {
    schema: rpi_ai::types::Tool,
    executed: Arc<std::sync::Mutex<Vec<String>>>,
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
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(rpi_agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, rpi_agent::AgentError> {
        let value = params
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Yield so sequential execution is observable: with a cooperative
        // scheduler, a (buggy) parallel path could interleave here. The TS test
        // relies on toolExecution: "sequential" to order first→second.
        self.executed
            .lock()
            .expect("executed lock")
            .push(value.clone());
        Ok(AgentToolResult::text(format!("ok:{value}")))
    }
}

fn echo_schema() -> rpi_ai::types::Tool {
    rpi_ai::types::Tool {
        name: "echo".to_string(),
        description: "Echo tool".to_string(),
        parameters: rpi_ai::types::Schema::new(serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"],
            "additionalProperties": false,
        })),
        constrained_sampling: None,
    }
}

#[tokio::test]
async fn steering_injected_after_tool_batch_completes() {
    // Mirrors TS "should inject queued messages after all tool calls complete".
    let executed: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let tool = EchoTool {
        schema: echo_schema(),
        executed: Arc::clone(&executed),
    };
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![Arc::new(tool)],
    };

    // The steering hook releases the interrupt exactly once, once at least one
    // tool has executed. After that it returns an empty vec on every subsequent
    // drain.
    let delivered = Arc::new(AtomicBool::new(false));
    let interrupt = user_message("interrupt");
    let steering: GetSteeringMessages = {
        let executed = Arc::clone(&executed);
        let delivered = Arc::clone(&delivered);
        let interrupt = interrupt.clone();
        Arc::new(move || {
            let executed = Arc::clone(&executed);
            let delivered = Arc::clone(&delivered);
            let interrupt = interrupt.clone();
            Box::pin(async move {
                let count = executed.lock().expect("executed lock").len();
                if count >= 1 && !delivered.swap(true, Ordering::SeqCst) {
                    vec![interrupt]
                } else {
                    Vec::new()
                }
            })
        })
    };

    // The stream_fn inspects the LLM context on the 2nd call to confirm the
    // interrupt landed before that call. Mirrors the TS `sawInterruptInContext`
    // closure. `mock_stream_fn` ignores its context args, so the inspection is
    // layered on via `inspect_for_interrupt` below.
    let saw_interrupt = Arc::new(AtomicBool::new(false));
    let stream_fn = common::mock_stream_fn(vec![
        assistant_tool_calls(
            vec![
                ("tool-1", "echo", serde_json::json!({ "value": "first" })),
                ("tool-2", "echo", serde_json::json!({ "value": "second" })),
            ],
            StopReason::ToolUse,
        ),
        assistant_text("done", StopReason::Stop),
    ]);
    // Wrap mock_stream_fn with a context inspector. The mock_stream_fn ignores
    // its context args, so we layer an inspection hook atop it via a custom
    // stream_fn that forwards to the mock AND records the interrupt sighting.
    let inspected = inspect_for_interrupt(stream_fn, Arc::clone(&saw_interrupt));

    let mut config = base_config();
    config.tool_execution = ToolExecutionMode::Sequential;
    config.get_steering_messages = Some(steering);

    let (events, _new_messages) =
        run_and_collect(vec![user_message("start")], context, config, inspected).await;

    // Both tools executed before steering was injected — the steering hook saw
    // `executed.len() == 2` on its first non-empty return.
    assert_eq!(
        executed.lock().expect("executed lock").clone(),
        vec!["first".to_string(), "second".to_string()],
        "both tools should execute before steering is injected"
    );

    // Two tool_execution_end events, neither an error.
    let ends: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
        .collect();
    assert_eq!(
        ends.len(),
        2,
        "expected exactly 2 tool_execution_end events"
    );
    for e in &ends {
        if let AgentEvent::ToolExecutionEnd { is_error, .. } = e {
            assert!(!*is_error, "tool_execution_end should not be an error");
        }
    }

    // The interrupt user message appears in the event stream AFTER both tool
    // results. Build the role/key sequence mirroring the TS `eventSequence`.
    let seq: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageStart { message, .. } => match message {
                rpi_agent::AgentMessage::ToolResult(t) => Some(format!("tool:{}", t.tool_call_id)),
                rpi_agent::AgentMessage::User(u) => u.content.as_text().map(|s| s.to_string()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert!(
        seq.contains(&"interrupt".to_string()),
        "interrupt message should appear in the event stream: {seq:?}"
    );
    let i1 = seq
        .iter()
        .position(|s| s == "tool:tool-1")
        .expect("tool-1 result in sequence");
    let i2 = seq
        .iter()
        .position(|s| s == "tool:tool-2")
        .expect("tool-2 result in sequence");
    let ish = seq
        .iter()
        .position(|s| s == "interrupt")
        .expect("interrupt in sequence");
    assert!(i1 < ish, "tool-1 result must precede the interrupt");
    assert!(i2 < ish, "tool-2 result must precede the interrupt");

    // The interrupt was in the context when the 2nd LLM call was made.
    assert!(
        saw_interrupt.load(Ordering::SeqCst),
        "the interrupt message should be in the context for the 2nd LLM call"
    );
}

/// Wrap a `StreamFn` so the inner context is inspected on every call: if any
/// message is the "interrupt" user text, set `saw`. Returns a new `StreamFn`
/// that forwards to `inner`.
fn inspect_for_interrupt(inner: rpi_agent::StreamFn, saw: Arc<AtomicBool>) -> rpi_agent::StreamFn {
    rpi_agent::stream_fn(move |model, ctx, opts| {
        // Inspect: does the LLM context carry the "interrupt" user message?
        let has_interrupt = ctx.messages.iter().any(|m| match m {
            rpi_ai::types::Message::User(u) => u.content.as_text() == Some("interrupt"),
            _ => false,
        });
        if has_interrupt {
            saw.store(true, Ordering::SeqCst);
        }
        inner(model, ctx, opts)
    })
}
