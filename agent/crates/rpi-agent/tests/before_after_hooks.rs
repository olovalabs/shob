//! before/after tool-call hooks (plan §2.3).
//!
//! Mirrors TS `agent-loop.test.ts`:
//! - "should handle tool calls and results" — `after_tool_call` observes the
//!   executed `usage` and overrides it; the final `ToolExecutionEnd.result.usage`
//!   and the persisted `ToolResultMessage.usage` carry the patched value.
//! - "should execute mutated beforeToolCall args without revalidation" —
//!   `before_tool_call` returns replacement `args` (a number where the schema
//!   requires a string); the replacement is applied WITHOUT re-validation, so
//!   `execute` sees the mutated value.
//! - "should stop after a blocked tool call when beforeToolCall sets
//!   terminate=true" — `before_tool_call` blocks + terminates; the tool never
//!   executes, exactly one LLM call runs, and the tool result is an error whose
//!   text is the block reason.
//! - "should continue after a mixed batch with one terminating blocked call" —
//!   a parallel batch where `before_tool_call` blocks "first" (terminate) but
//!   lets "second" run; the loop continues for a second LLM turn.

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;

use common::{assistant_text, assistant_tool_calls, base_config, run_and_collect, user_message};
use rpi_agent::{
    AfterToolCall, AfterToolCallResult, AgentContext, AgentEvent, AgentToolResult, BeforeToolCall,
    BeforeToolCallResult, ToolExecutionMode,
};
use rpi_ai::types::{StopReason, Usage, UsageCost};
use tokio_util::sync::CancellationToken;

// ----------------------------------------------------------------------------
// Shared echo tool — records every executed `value` (as a raw JSON value so the
// mutate-args test can observe a number where a string was validated).
// ----------------------------------------------------------------------------

/// Shared recording buffer for the echo tool.
type Executed = Arc<std::sync::Mutex<Vec<serde_json::Value>>>;

/// A scripted echo tool — records every executed `value` (as a raw JSON value
/// so the mutate-args test can observe a number where a string was validated).
struct EchoTool {
    schema: rpi_ai::types::Tool,
    executed: Executed,
    /// When `Some`, `execute` returns this `usage` on its result. The
    /// `after_tool_call` test uses this to assert the hook observes it.
    usage: Option<Usage>,
}

impl EchoTool {
    fn new_recording() -> (Self, Executed) {
        let executed: Executed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tool = EchoTool {
            schema: value_schema("echo"),
            executed: Arc::clone(&executed),
            usage: None,
        };
        (tool, executed)
    }

    fn new_with_usage(usage: Usage) -> (Self, Executed) {
        let executed: Executed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tool = EchoTool {
            schema: value_schema("echo"),
            executed: Arc::clone(&executed),
            usage: Some(usage),
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
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(rpi_agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, rpi_agent::AgentError> {
        let value = params
            .get("value")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        self.executed
            .lock()
            .expect("executed lock")
            .push(value.clone());
        let mut result = AgentToolResult::text(format!("echoed: {value}"));
        result.details = serde_json::json!({ "value": value });
        if let Some(u) = &self.usage {
            result.usage = Some(u.clone());
        }
        Ok(result)
    }
}

fn value_schema(name: &str) -> rpi_ai::types::Tool {
    rpi_ai::types::Tool {
        name: name.to_string(),
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

fn context_with_tools(tools: Vec<Arc<dyn rpi_agent::AgentTool>>) -> AgentContext {
    AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools,
    }
}

/// Count `turn_start` events — each turn is exactly one LLM call, so this is the
/// Rust equivalent of the TS `llmCalls` counter.
fn turn_start_count(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::TurnStart))
        .count()
}

#[tokio::test]
async fn parallel_tools_preserve_history_and_after_hook_overrides() {
    let history = user_message("retained history ".repeat(4096));
    for with_hook in [false, true] {
        let (tool, executed) = EchoTool::new_recording();
        let mut context = context_with_tools(vec![Arc::new(tool)]);
        context.messages.push(history.clone());
        context.system_prompt = "keep the system prompt".into();
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed_contexts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut config = base_config();
        config.tool_execution = ToolExecutionMode::Parallel;
        if with_hook {
            let expected_history = history.clone();
            let observed = observed.clone();
            let observed_contexts = observed_contexts.clone();
            config.after_tool_call = Some(Arc::new(move |ctx, _| {
                assert_eq!(ctx.context.messages[0], expected_history);
                assert_eq!(ctx.context.system_prompt, "keep the system prompt");
                assert_eq!(ctx.context.tools.len(), 1);
                assert!(
                    matches!(ctx.context.messages.last(), Some(rpi_agent::AgentMessage::Assistant(a)) if **a == *ctx.assistant_message)
                );
                observed_contexts
                    .lock()
                    .unwrap()
                    .push(ctx.context.messages.as_ptr() as usize);
                observed.lock().unwrap().push(ctx.tool_call.id.clone());
                Box::pin(async {
                    Some(AfterToolCallResult {
                        details: Some(serde_json::json!({"after": true})),
                        ..Default::default()
                    })
                })
            }));
        }
        let stream = common::mock_stream_fn(vec![
            assistant_tool_calls(
                vec![
                    ("a", "echo", serde_json::json!({"value": "one"})),
                    ("b", "echo", serde_json::json!({"value": "two"})),
                ],
                StopReason::ToolUse,
            ),
            assistant_text("done", StopReason::Stop),
        ]);
        let (_, messages) =
            run_and_collect(vec![user_message("run both")], context, config, stream).await;
        let results: Vec<_> = messages
            .iter()
            .filter_map(|message| match message {
                rpi_agent::AgentMessage::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect();
        assert_eq!(
            results
                .iter()
                .map(|r| r.tool_call_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert!(results.iter().all(|r| !r.is_error));
        assert_eq!(executed.lock().unwrap().len(), 2);
        assert_eq!(
            observed.lock().unwrap().len(),
            if with_hook { 2 } else { 0 }
        );
        if with_hook {
            let contexts = observed_contexts.lock().unwrap();
            assert_eq!(contexts.len(), 2);
            assert_eq!(contexts[0], contexts[1]);
        }
        for result in results {
            if with_hook {
                assert_eq!(result.details, Some(serde_json::json!({"after": true})));
            } else {
                assert!(result.details.as_ref().unwrap().get("value").is_some());
            }
        }
    }
}

// ----------------------------------------------------------------------------
// after_tool_call usage override
// ----------------------------------------------------------------------------

/// Build a `Usage` with the 4 token fields + total + a full cost block. `cache_write_1h`
/// and `reasoning` are `None` (not exercised here).
fn usage_block(
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    total: i64,
    cost: (f64, f64, f64, f64, f64),
) -> Usage {
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: total,
        cost: UsageCost {
            input: cost.0,
            output: cost.1,
            cache_read: cost.2,
            cache_write: cost.3,
            total: cost.4,
        },
    }
}

#[tokio::test]
async fn after_tool_call_overrides_usage() {
    // Mirrors TS "should handle tool calls and results". The tool returns
    // `toolUsage` from execute; the afterToolCall hook observes that usage on
    // the executed result and returns `{ usage: patchedToolUsage }`; the final
    // ToolExecutionEnd + ToolResultMessage carry the patched usage.
    let tool_usage = usage_block(1, 2, 3, 4, 10, (0.1, 0.2, 0.3, 0.4, 1.0));
    let patched_usage = usage_block(5, 6, 7, 8, 26, (0.5, 0.6, 0.7, 0.8, 2.6));

    let observed: Arc<std::sync::Mutex<Option<Usage>>> = Arc::new(std::sync::Mutex::new(None));
    let (tool, executed) = EchoTool::new_with_usage(tool_usage.clone());
    let context = context_with_tools(vec![Arc::new(tool)]);

    let after: AfterToolCall = {
        let observed = Arc::clone(&observed);
        let patched = patched_usage.clone();
        Arc::new(
            move |ctx: rpi_agent::AfterToolCallContext<'_>, _signal: CancellationToken| {
                let observed = Arc::clone(&observed);
                let patched = patched.clone();
                // Clone out of the borrowed context BEFORE the async block — the
                // BoxFuture is `'static`, so it can't hold the `'_` borrow.
                let observed_usage = ctx.result.usage.clone();
                Box::pin(async move {
                    *observed.lock().expect("observed lock") = observed_usage;
                    Some(AfterToolCallResult {
                        usage: Some(patched),
                        ..AfterToolCallResult::default()
                    })
                })
            },
        )
    };

    let mut config = base_config();
    config.after_tool_call = Some(after);

    // call 1: a single tool call; call 2: text "done".
    let stream_fn = common::mock_stream_fn(vec![
        assistant_tool_calls(
            vec![("tool-1", "echo", serde_json::json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        assistant_text("done", StopReason::Stop),
    ]);
    let (events, new_messages) = run_and_collect(
        vec![user_message("echo something")],
        context,
        config,
        stream_fn,
    )
    .await;

    // The tool ran once with the validated string args.
    assert_eq!(
        executed.lock().expect("executed lock").clone(),
        vec![serde_json::json!("hello")],
    );

    // The hook observed the tool's own usage (pre-override).
    assert_eq!(
        observed.lock().expect("observed lock").clone(),
        Some(tool_usage),
        "after_tool_call must observe the executed result's usage before overriding"
    );

    // ToolExecutionStart + ToolExecutionEnd both present; end is not an error.
    let start = events
        .iter()
        .find(|e| matches!(e, AgentEvent::ToolExecutionStart { .. }));
    let end = events
        .iter()
        .find(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }));
    assert!(start.is_some(), "missing tool_execution_start");
    assert!(end.is_some(), "missing tool_execution_end");
    if let Some(AgentEvent::ToolExecutionEnd {
        is_error, result, ..
    }) = end
    {
        assert!(!*is_error, "tool_execution_end should not be an error");
        assert_eq!(
            result.usage,
            Some(patched_usage.clone()),
            "ToolExecutionEnd.result.usage must be the patched usage"
        );
    }

    // The persisted ToolResultMessage carries the patched usage too.
    let tool_result = new_messages.iter().find_map(|m| match m {
        rpi_agent::AgentMessage::ToolResult(t) => Some(t),
        _ => None,
    });
    let tool_result = tool_result.expect("a toolResult message");
    assert!(!tool_result.is_error, "toolResult should not be an error");
    assert_eq!(
        tool_result.usage,
        Some(patched_usage),
        "ToolResultMessage.usage must be the patched usage"
    );
}

// ----------------------------------------------------------------------------
// before_tool_call mutates args without revalidation
// ----------------------------------------------------------------------------

#[tokio::test]
async fn before_tool_call_mutates_args_without_revalidation() {
    // Mirrors TS "should execute mutated beforeToolCall args without
    // revalidation". The schema requires `value: string`; validation passes on
    // `"hello"`; beforeToolCall returns a replacement `{"value": 123}` (a
    // number). The replacement is applied WITHOUT re-validation, so execute
    // sees `123`. In TS the callback mutates `args` in place and returns
    // undefined; Rust hands an immutable borrow, so the rewrite is signalled by
    // `BeforeToolCallResult::args` — the behavior under test is identical.
    let (tool, executed) = EchoTool::new_recording();
    let context = context_with_tools(vec![Arc::new(tool)]);

    let before: BeforeToolCall = Arc::new(
        |_ctx: rpi_agent::BeforeToolCallContext<'_>, _signal: CancellationToken| {
            Box::pin(async move {
                Some(BeforeToolCallResult {
                    args: Some(serde_json::json!({ "value": 123 })),
                    ..BeforeToolCallResult::default()
                })
            })
        },
    );

    let mut config = base_config();
    config.before_tool_call = Some(before);

    let stream_fn = common::mock_stream_fn(vec![
        assistant_tool_calls(
            vec![("tool-1", "echo", serde_json::json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        assistant_text("done", StopReason::Stop),
    ]);
    let (_events, _new_messages) = run_and_collect(
        vec![user_message("echo something")],
        context,
        config,
        stream_fn,
    )
    .await;

    // execute received the mutated number, not the validated string.
    assert_eq!(
        executed.lock().expect("executed lock").clone(),
        vec![serde_json::json!(123)],
        "before_tool_call args replacement must be applied without re-validation"
    );
}

// ----------------------------------------------------------------------------
// before_tool_call block + terminate stops the loop
// ----------------------------------------------------------------------------

#[tokio::test]
async fn before_tool_call_block_terminate_stops_loop() {
    // Mirrors TS "should stop after a blocked tool call when beforeToolCall sets
    // terminate=true". The tool never executes; exactly one LLM call runs; the
    // tool result is an error whose text is the block reason.
    let (tool, executed) = EchoTool::new_recording();
    let context = context_with_tools(vec![Arc::new(tool)]);

    let before: BeforeToolCall = Arc::new(
        |_ctx: rpi_agent::BeforeToolCallContext<'_>, _signal: CancellationToken| {
            Box::pin(async move {
                Some(BeforeToolCallResult {
                    block: true,
                    reason: Some("Blocked by policy".to_string()),
                    terminate: true,
                    ..BeforeToolCallResult::default()
                })
            })
        },
    );

    let mut config = base_config();
    config.before_tool_call = Some(before);

    let stream_fn = common::mock_stream_fn(vec![
        assistant_tool_calls(
            vec![("tool-1", "echo", serde_json::json!({ "value": "hello" }))],
            StopReason::ToolUse,
        ),
        // "should not run" — if the loop wrongly continues, this is call 2.
        assistant_text("should not run", StopReason::Stop),
    ]);
    let (events, new_messages) = run_and_collect(
        vec![user_message("echo something")],
        context,
        config,
        stream_fn,
    )
    .await;

    // The tool did not execute.
    assert!(
        executed.lock().expect("executed lock").is_empty(),
        "blocked tool must not execute"
    );

    // Exactly one LLM call (one turn).
    assert_eq!(
        turn_start_count(&events),
        1,
        "loop must stop after a blocked+terminate tool call"
    );

    // The tool result is an error carrying the block reason.
    let tool_result = new_messages.iter().find_map(|m| match m {
        rpi_agent::AgentMessage::ToolResult(t) => Some(t),
        _ => None,
    });
    let tool_result = tool_result.expect("a toolResult message");
    assert!(tool_result.is_error, "blocked tool result must be an error");
    let text = tool_result
        .content
        .iter()
        .filter_map(|c| match c {
            rpi_ai::types::Content::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert!(
        text.contains("Blocked by policy"),
        "blocked tool result text must be the block reason, got: {text:?}"
    );
}

// ----------------------------------------------------------------------------
// before_tool_call mixed batch — one blocked+terminate, one runs — continues
// ----------------------------------------------------------------------------

#[tokio::test]
async fn before_tool_call_mixed_batch_continues() {
    // Mirrors TS "should continue after a mixed batch with one terminating
    // blocked call". A parallel batch: "first" is blocked+terminate, "second"
    // runs. Because not-all-terminate, the batch continues; a second LLM turn
    // ("done") runs. executed == ["second"], 2 LLM calls.
    let (tool, executed) = EchoTool::new_recording();
    let context = context_with_tools(vec![Arc::new(tool)]);

    let before: BeforeToolCall = {
        Arc::new(
            |ctx: rpi_agent::BeforeToolCallContext<'_>, _signal: CancellationToken| {
                let value = ctx
                    .args
                    .get("value")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Box::pin(async move {
                    if value == "first" {
                        Some(BeforeToolCallResult {
                            block: true,
                            reason: Some("Blocked first".to_string()),
                            terminate: true,
                            ..BeforeToolCallResult::default()
                        })
                    } else {
                        None
                    }
                })
            },
        )
    };

    let mut config = base_config();
    config.before_tool_call = Some(before);
    config.tool_execution = ToolExecutionMode::Parallel;

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
    let (events, _new_messages) =
        run_and_collect(vec![user_message("echo both")], context, config, stream_fn).await;

    // Only "second" executed.
    assert_eq!(
        executed.lock().expect("executed lock").clone(),
        vec![serde_json::json!("second")],
        "only the non-blocked call should execute"
    );

    // The loop continued for a second turn.
    assert_eq!(
        turn_start_count(&events),
        2,
        "mixed batch with not-all-terminate must continue for a second LLM turn"
    );
}
