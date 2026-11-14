//! Truncate-fail invariant (plan §5.2).
//!
//! Mirrors TS `agent-loop.test.ts`:
//! "should not execute tool calls when the stop reason is length".
//!
//! When the assistant message carries `stop_reason: Length` AND contains a tool
//! call, the loop must:
//! - NOT execute the tool (the salvage parser may have produced truncated args).
//! - Emit `ToolExecutionEnd` with `is_error: true` whose text mentions
//!   "output token limit".
//! - Keep the loop going so the model can re-issue the call (callIndex == 2:
//!   the truncated turn + a follow-up assistant turn).

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;

use common::{base_config, run_and_collect, user_message};
use rpi_agent::{AgentContext, AgentEvent, AgentToolResult};
use rpi_ai::types::StopReason;

/// An echo tool that records every executed `value` — must stay empty on a
/// truncated turn.
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
        _signal: tokio_util::sync::CancellationToken,
        _on_update: Arc<dyn Fn(rpi_agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, rpi_agent::AgentError> {
        let value = params
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        self.executed
            .lock()
            .expect("executed lock")
            .push(value.clone());
        Ok(AgentToolResult::text(format!("echoed: {value}")))
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
async fn stop_reason_length_fails_tool_calls_without_executing() {
    use common::assistant_text;
    use common::mock_stream_fn;

    let executed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let tool = EchoTool {
        schema: echo_schema(),
        executed: Arc::clone(&executed),
    };
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![Arc::new(tool)],
    };

    // call 1: a tool-call message whose stop_reason is Length. The mock
    // supplies already-truncated args (`value: "hel"`); the loop must refuse
    // to execute.
    let truncated = assistant_message_with_tool_call_and_length();
    // call 2: a plain text reply that lets the loop settle.
    let followup = assistant_text("done", StopReason::Stop);
    let stream_fn = mock_stream_fn(vec![truncated, followup]);

    let (events, new_messages) = run_and_collect(
        vec![user_message("echo something")],
        context,
        base_config(),
        stream_fn,
    )
    .await;

    // The tool MUST NOT have executed.
    assert!(
        executed.lock().expect("executed lock").is_empty(),
        "tool executed on a truncated turn: {:?}",
        executed.lock().expect("executed lock")
    );

    // There is exactly one ToolExecutionEnd and it's an error mentioning the
    // output token limit.
    let ends: Vec<&AgentEvent> = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolExecutionEnd { .. }))
        .collect();
    assert_eq!(
        ends.len(),
        1,
        "expected exactly one tool_execution_end, got {}",
        ends.len()
    );
    if let AgentEvent::ToolExecutionEnd {
        is_error, result, ..
    } = ends[0]
    {
        assert!(*is_error, "truncated tool end must be an error");
        let text = result
            .content
            .iter()
            .filter_map(|c| match c {
                rpi_agent::TextContentOrImage::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(
            text.contains("output token limit"),
            "error text must mention 'output token limit', got: {text:?}"
        );
    }

    // The loop continued: 2 LLM calls → 3 new messages (prompt + truncated
    // assistant + tool result ... + follow-up assistant). The last new message
    // is the follow-up assistant reply.
    let last = new_messages
        .last()
        .expect("at least the follow-up assistant");
    assert_eq!(
        last.role().as_str(),
        "assistant",
        "loop should continue past the truncated turn"
    );
}

/// Build the truncated assistant message: one tool call + `stop_reason: Length`.
fn assistant_message_with_tool_call_and_length() -> rpi_ai::types::AssistantMessage {
    use rpi_ai::types::{Api, AssistantMessage, Content, ToolCall, Usage};
    AssistantMessage {
        role: rpi_ai::types::AssistantRole,
        content: vec![Content::ToolCall(ToolCall {
            kind: rpi_ai::types::ToolCallType,
            id: "tool-1".to_string(),
            name: "echo".to_string(),
            arguments: serde_json::json!({ "value": "hel" }),
            thought_signature: None,
            namespace: None,
            recorded: None,
        })],
        api: Api::Other("openai-responses".into()),
        provider: "mock".to_string(),
        model: "mock".to_string(),
        response_model: None,
        response_id: None,
        usage: Usage::zero(),
        stop_reason: StopReason::Length,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    }
}
