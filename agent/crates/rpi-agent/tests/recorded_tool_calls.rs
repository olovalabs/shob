//! Recorded tool calls: a streamed call whose side effect already happened
//! upstream (a remote agent ran the tool and streamed back its recording).
//!
//! The local loop must never execute such a call. Its recorded result becomes
//! the tool result; an invalid or unsupported recording becomes an error result.
//! Either way the tool's `execute` must not run.

#[path = "common/mod.rs"]
mod common;

use std::sync::{Arc, Mutex};

use common::{base_config, mock_stream_fn, run_and_collect, user_message};
use rpi_agent::{AgentContext, AgentError, AgentTool, AgentToolResult, ToolResultPartial};
use rpi_ai::types::{Api, AssistantMessage, Content, StopReason, Tool, Usage};
use tokio_util::sync::CancellationToken;

/// A `shell`-like tool whose `execute` is observable: every real run bumps
/// `executions`.
struct ShellTool {
    schema: Tool,
    executions: Arc<Mutex<usize>>,
}

impl ShellTool {
    fn new() -> (Self, Arc<Mutex<usize>>) {
        let executions = Arc::new(Mutex::new(0));
        let schema = Tool {
            name: "shell".to_string(),
            description: "Run a shell command".to_string(),
            parameters: rpi_ai::types::Schema::new(serde_json::json!({
                "type": "object",
                "properties": { "command": { "type": "string" } },
                "required": ["command"],
                "additionalProperties": false
            })),
            constrained_sampling: None,
        };
        (
            ShellTool {
                schema,
                executions: Arc::clone(&executions),
            },
            executions,
        )
    }
}

#[async_trait::async_trait]
impl AgentTool for ShellTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "Shell"
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _params: serde_json::Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        *self.executions.lock().expect("executions lock") += 1;
        Ok(AgentToolResult::text("executed locally"))
    }
}

/// A `toolCall` content block as a provider stream delivers it. It is built
/// from JSON so the test compiles against the wire shape, not a Rust literal.
fn streamed_call(id: &str, command: &str, recorded: Option<serde_json::Value>) -> Content {
    let mut block = serde_json::json!({
        "type": "toolCall",
        "id": id,
        "name": "shell",
        "arguments": { "command": command },
    });
    if let Some(recorded) = recorded {
        block["recorded"] = recorded;
    }
    serde_json::from_value(block).expect("streamed tool call decodes")
}

/// A recording in the shape the Cursor replay worker emits (`replay.ts`):
/// `{"version":1,"isError":bool,"result":{"content":[...],"details":{},"terminate"?:bool}}`.
fn worker_recording(text: &str, is_error: bool, terminate: Option<bool>) -> serde_json::Value {
    let mut result = serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "details": {}
    });
    if let Some(terminate) = terminate {
        result["terminate"] = serde_json::json!(terminate);
    }
    serde_json::json!({ "version": 1, "isError": is_error, "result": result })
}

fn assistant_with(content: Vec<Content>) -> AssistantMessage {
    AssistantMessage {
        role: rpi_ai::types::AssistantRole,
        content,
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
    }
}

/// Run one assistant turn carrying `content`, then return the executions count
/// and every tool result the loop committed, in order.
async fn run_turn(content: Vec<Content>) -> (usize, Vec<rpi_ai::types::ToolResultMessage>) {
    run_turn_with(content, None, None).await
}

/// [`run_turn`] with optional `before_tool_call` and `after_tool_call` hooks.
async fn run_turn_with(
    content: Vec<Content>,
    before: Option<rpi_agent::BeforeToolCall>,
    after: Option<rpi_agent::AfterToolCall>,
) -> (usize, Vec<rpi_ai::types::ToolResultMessage>) {
    let (tool, executions) = ShellTool::new();
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![Arc::new(tool)],
    };
    let script = vec![
        assistant_with(content),
        common::assistant_text("done", StopReason::Stop),
    ];
    let mut config = base_config();
    config.before_tool_call = before;
    config.after_tool_call = after;
    let (_events, new_messages) = run_and_collect(
        vec![user_message("run it")],
        context,
        config,
        mock_stream_fn(script),
    )
    .await;
    let results = new_messages
        .into_iter()
        .filter_map(|message| match message {
            rpi_agent::AgentMessage::ToolResult(result) => Some(*result),
            _ => None,
        })
        .collect();
    let executed = *executions.lock().expect("executions lock");
    (executed, results)
}

fn text_of(result: &rpi_ai::types::ToolResultMessage) -> String {
    common::tool_result_text(result)
}

#[tokio::test]
async fn recorded_call_is_never_executed_and_its_recorded_result_is_used() {
    let recording = worker_recording("recorded output", false, Some(true));
    let (executed, results) = run_turn(vec![streamed_call(
        "call-1",
        "rm -rf target",
        Some(recording),
    )])
    .await;

    assert_eq!(executed, 0, "a recorded call must not run locally");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].tool_call_id, "call-1");
    assert!(!results[0].is_error);
    assert_eq!(text_of(&results[0]), "recorded output");
}

#[tokio::test]
async fn recorded_terminate_stops_the_loop_like_a_live_terminate() {
    // `terminate: true` ends the batch, so the scripted follow-up answer is never
    // requested. Without it the loop asks the model again.
    for (terminate, assistant_turns) in [(Some(true), 1), (Some(false), 2), (None, 2)] {
        let recording = worker_recording("recorded output", false, terminate);
        let (tool, _executions) = ShellTool::new();
        let context = AgentContext {
            system_prompt: String::new(),
            messages: Vec::new(),
            tools: vec![Arc::new(tool)],
        };
        let script = vec![
            assistant_with(vec![streamed_call(
                "call-1",
                "rm -rf target",
                Some(recording),
            )]),
            common::assistant_text("done", StopReason::Stop),
        ];
        let (_events, new_messages) = run_and_collect(
            vec![user_message("run it")],
            context,
            base_config(),
            mock_stream_fn(script),
        )
        .await;
        let assistants = new_messages
            .iter()
            .filter(|m| matches!(m, rpi_agent::AgentMessage::Assistant(_)))
            .count();
        assert_eq!(assistants, assistant_turns, "terminate {terminate:?}");
    }
}

#[tokio::test]
async fn recorded_error_result_is_preserved_without_execution() {
    let recording = worker_recording("exit 1", true, None);
    let (executed, results) =
        run_turn(vec![streamed_call("call-1", "false", Some(recording))]).await;

    assert_eq!(executed, 0);
    assert!(results[0].is_error);
    assert_eq!(text_of(&results[0]), "exit 1");
}

#[tokio::test]
async fn unsupported_recording_version_is_an_error_and_never_executes() {
    let recording = serde_json::json!({
        "version": 2,
        "isError": false,
        "result": { "content": [{ "type": "text", "text": "from the future" }] }
    });
    let (executed, results) = run_turn(vec![streamed_call(
        "call-1",
        "rm -rf target",
        Some(recording),
    )])
    .await;

    assert_eq!(executed, 0, "an unsupported recording must not run locally");
    assert!(results[0].is_error);
    assert!(text_of(&results[0]).contains("Unsupported recorded tool result version"));
}

#[tokio::test]
async fn malformed_recordings_are_errors_and_never_execute() {
    let malformed = [
        // A present-but-null recording is still a recording, not an ordinary call.
        serde_json::Value::Null,
        serde_json::json!("not an object"),
        serde_json::json!({ "isError": false, "result": { "content": [] } }),
        // A partial recording, persisted before the worker recorded the result.
        serde_json::json!({ "version": 1 }),
        // The flat shape is not the wire shape: there is no `result` object.
        serde_json::json!({ "version": 1, "isError": false, "content": [{ "type": "text", "text": "flat" }] }),
        serde_json::json!({ "version": 1, "isError": false, "result": { "content": [] }, "extra": true }),
        serde_json::json!({ "version": 1, "isError": false, "result": { "content": [], "terminate": "yes" } }),
        serde_json::json!({
            "version": 1,
            "isError": false,
            "result": { "content": [{ "type": "toolCall", "id": "x", "name": "shell", "arguments": {} }] }
        }),
    ];
    for recording in malformed {
        let (executed, results) = run_turn(vec![streamed_call(
            "call-1",
            "rm -rf target",
            Some(recording.clone()),
        )])
        .await;
        assert_eq!(executed, 0, "malformed recording {recording} must not run");
        assert_eq!(results.len(), 1);
        assert!(
            results[0].is_error,
            "malformed recording {recording} is an error"
        );
    }
}

#[tokio::test]
async fn ordinary_calls_still_execute_next_to_a_recorded_call() {
    let recording = worker_recording("recorded output", false, Some(true));
    let (executed, results) = run_turn(vec![
        streamed_call("recorded", "rm -rf target", Some(recording)),
        streamed_call("ordinary", "echo hi", None),
    ])
    .await;

    assert_eq!(executed, 1, "only the ordinary call runs");
    assert_eq!(results.len(), 2);
    let by_id = |id: &str| results.iter().find(|r| r.tool_call_id == id).unwrap();
    assert_eq!(text_of(by_id("recorded")), "recorded output");
    assert_eq!(text_of(by_id("ordinary")), "executed locally");
    assert!(!by_id("ordinary").is_error);
}

/// An explicit retry is a new call with no recording, so it runs, even when it
/// repeats a recorded call's arguments. A recording is never inherited.
#[tokio::test]
async fn an_explicit_retry_of_a_recorded_call_runs_once() {
    let (tool, executions) = ShellTool::new();
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![Arc::new(tool)],
    };
    let script = vec![
        assistant_with(vec![streamed_call(
            "replayed",
            "git push",
            Some(worker_recording("rejected", true, None)),
        )]),
        assistant_with(vec![streamed_call("retry", "git push", None)]),
        common::assistant_text("done", StopReason::Stop),
    ];
    let (_events, new_messages) = run_and_collect(
        vec![user_message("push it")],
        context,
        base_config(),
        mock_stream_fn(script),
    )
    .await;
    let results: Vec<_> = new_messages
        .into_iter()
        .filter_map(|message| match message {
            rpi_agent::AgentMessage::ToolResult(result) => Some(*result),
            _ => None,
        })
        .collect();

    assert_eq!(*executions.lock().expect("executions lock"), 1);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].tool_call_id, "replayed");
    assert!(results[0].is_error);
    assert_eq!(text_of(&results[0]), "rejected");
    assert_eq!(results[1].tool_call_id, "retry");
    assert_eq!(text_of(&results[1]), "executed locally");
}

#[tokio::test]
async fn approval_hooks_never_see_or_gate_a_recorded_call() {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let hook_seen = Arc::clone(&seen);
    let hook: rpi_agent::BeforeToolCall = Arc::new(move |ctx, _signal| {
        let id = ctx.tool_call.id.clone();
        hook_seen.lock().expect("seen lock").push(id);
        Box::pin(async move { None })
    });
    let recording = worker_recording("recorded output", false, Some(true));
    let (executed, results) = run_turn_with(
        vec![
            streamed_call("recorded", "rm -rf target", Some(recording)),
            streamed_call("ordinary", "echo hi", None),
        ],
        Some(hook),
        None,
    )
    .await;

    assert_eq!(executed, 1);
    assert_eq!(
        *seen.lock().expect("seen lock"),
        vec!["ordinary".to_string()]
    );
    let recorded = results
        .iter()
        .find(|r| r.tool_call_id == "recorded")
        .unwrap();
    assert_eq!(text_of(recorded), "recorded output");
}

#[tokio::test]
async fn after_tool_call_never_sees_a_recorded_call() {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let hook_seen = Arc::clone(&seen);
    let hook: rpi_agent::AfterToolCall = Arc::new(move |ctx, _signal| {
        hook_seen
            .lock()
            .expect("seen lock")
            .push(ctx.tool_call.id.clone());
        Box::pin(async move { None })
    });
    let recording = worker_recording("recorded output", false, Some(true));
    let (executed, results) = run_turn_with(
        vec![
            streamed_call("recorded", "rm -rf target", Some(recording)),
            streamed_call("ordinary", "echo hi", None),
        ],
        None,
        Some(hook),
    )
    .await;

    assert_eq!(executed, 1);
    assert_eq!(
        *seen.lock().expect("seen lock"),
        vec!["ordinary".to_string()]
    );
    let recorded = results
        .iter()
        .find(|r| r.tool_call_id == "recorded")
        .unwrap();
    assert_eq!(text_of(recorded), "recorded output");
}
