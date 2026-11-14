//! `prepareArguments` (plan §2.3, mirrors TS `AgentTool.prepareArguments`).
//!
//! Mirrors TS `agent-loop.test.ts`:
//! "should prepare tool arguments for validation".
//!
//! An `edit` tool whose schema requires `{ edits: [{ oldText, newText }] }`
//! receives a raw call with the legacy `{ oldText, newText }` shape.
//! `prepare_arguments` folds the legacy pair into an `edits` array BEFORE schema
//! validation runs, so the validated args (and what `execute` sees) is
//! `{ edits: [{ oldText: "before", newText: "after" }] }`.

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;

use common::{assistant_text, base_config, mock_stream_fn, run_and_collect, user_message};
use rpi_agent::{AgentContext, AgentError, AgentTool, AgentToolResult, ToolResultPartial};
use rpi_ai::types::StopReason;
use tokio_util::sync::CancellationToken;

/// The `edit` tool. Mirrors the TS test tool: `prepare_arguments` folds legacy
/// `oldText`/`newText` into `edits`; `execute` records the `edits` array it
/// received.
struct EditTool {
    schema: rpi_ai::types::Tool,
    executed: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}

impl EditTool {
    fn new() -> (Self, Arc<std::sync::Mutex<Vec<serde_json::Value>>>) {
        let executed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let schema = rpi_ai::types::Tool {
            name: "edit".to_string(),
            description: "Edit tool".to_string(),
            parameters: rpi_ai::types::Schema::new(serde_json::json!({
                "type": "object",
                "properties": {
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "oldText": { "type": "string" },
                                "newText": { "type": "string" }
                            },
                            "required": ["oldText", "newText"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["edits"],
                "additionalProperties": false
            })),
            constrained_sampling: None,
        };
        let tool = EditTool {
            schema,
            executed: Arc::clone(&executed),
        };
        (tool, executed)
    }
}

#[async_trait::async_trait]
impl AgentTool for EditTool {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "Edit"
    }

    /// Fold legacy `oldText`/`newText` into `edits`. Faithful port of the TS
    /// `prepareArguments` in the test.
    fn prepare_arguments(&self, args: serde_json::Value) -> Result<serde_json::Value, AgentError> {
        let Some(obj) = args.as_object() else {
            return Ok(args);
        };
        let old_text = obj.get("oldText").and_then(|v| v.as_str());
        let new_text = obj.get("newText").and_then(|v| v.as_str());
        let (Some(old), Some(new)) = (old_text, new_text) else {
            return Ok(args);
        };
        let mut edits = obj
            .get("edits")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        edits.push(serde_json::json!({ "oldText": old, "newText": new }));
        Ok(serde_json::json!({ "edits": edits }))
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let edits = params
            .get("edits")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let count = edits.as_array().map(|a| a.len()).unwrap_or(0);
        self.executed.lock().expect("executed lock").push(edits);
        Ok(AgentToolResult::text(format!("edited {count}")))
    }
}

#[tokio::test]
async fn prepare_arguments_folds_legacy_oldtext_newtext_into_edits() {
    use rpi_ai::types::{Api, AssistantMessage, Content, ToolCall, Usage};

    let (tool, executed) = EditTool::new();
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![Arc::new(tool)],
    };

    // call 1: raw legacy args `{oldText, newText}`; call 2: text "done".
    let tool_use = AssistantMessage {
        role: rpi_ai::types::AssistantRole,
        content: vec![Content::ToolCall(ToolCall {
            kind: rpi_ai::types::ToolCallType,
            id: "tool-1".to_string(),
            name: "edit".to_string(),
            arguments: serde_json::json!({ "oldText": "before", "newText": "after" }),
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
        stop_reason: StopReason::ToolUse,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    };
    let done = assistant_text("done", StopReason::Stop);

    let stream_fn = mock_stream_fn(vec![tool_use, done]);
    let (_events, _new_messages) = run_and_collect(
        vec![user_message("edit something")],
        context,
        base_config(),
        stream_fn,
    )
    .await;

    // The edit tool saw the folded `edits` array — not the legacy shape.
    let executed = executed.lock().expect("executed lock").clone();
    assert_eq!(
        executed,
        vec![serde_json::json!([{"oldText": "before", "newText": "after"}])],
        "prepare_arguments should fold oldText/newText into edits before validation"
    );
}
