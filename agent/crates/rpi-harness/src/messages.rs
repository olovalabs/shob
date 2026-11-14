//! Mirrors `packages/agent/src/harness/messages.ts` — the harness's custom-role
//! `AgentMessage` constructors and the harness-level `convert_to_llm`.
//!
//! TS extends the agent's `AgentMessage` via declaration merging on
//! `CustomAgentMessages` with four roles: `bashExecution`, `custom`,
//! `branchSummary`, `compactionSummary`. Rust has no declaration merging, so
//! each role is a `rpi_agent::message::AgentMessage::Custom(CustomMessage)`
//! whose `role` field carries the TS role string and whose `data: Value` carries
//! the role-specific structured payload (mirrors the TS role interface fields).
//!
//! `convert_to_llm` is the *harness* converter (distinct from
//! `rpi_agent::default_convert_to_llm`, which drops custom roles): it projects
//! each custom role into a provider-facing `rpi_ai::Message` (a `User` message
//! with role-rendered text), so the LLM sees bash output / branch + compaction
//! summaries as user text. Unregistered custom roles are dropped (mirrors the
//! TS `default → undefined` branch).
//!
//! Pulled forward from M5e because M5d compaction depends on it
//! (`createCompactionSummaryMessage`, `createBranchSummaryMessage`,
//! `convertToLlm`).

use rpi_agent::message::{AgentMessage, CustomMessage};
use rpi_ai::types::{
    Content, Message, ToolResultMessage, ToolResultRole, UserContent, UserMessage,
};
use serde::{Deserialize, Serialize};

/// Wraps `summary` in the compaction-summary text envelope. Mirrors
/// `COMPACTION_SUMMARY_PREFIX` / `SUFFIX`.
pub const COMPACTION_SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

/// Wraps `summary` in the branch-summary text envelope. Mirrors
/// `BRANCH_SUMMARY_PREFIX` / `SUFFIX`.
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

// ---------------------------------------------------------------------------
// Role tag constants
// ---------------------------------------------------------------------------

pub const BASH_EXECUTION_ROLE: &str = "bashExecution";
pub const CUSTOM_ROLE: &str = "custom";
pub const BRANCH_SUMMARY_ROLE: &str = "branchSummary";
pub const COMPACTION_SUMMARY_ROLE: &str = "compactionSummary";

// ---------------------------------------------------------------------------
// Per-role structured payloads (stored on `CustomMessage::data`)
// ---------------------------------------------------------------------------

/// `BashExecutionMessage` payload. Mirrors the TS interface fields (minus
/// `role`/`timestamp`, which live on `CustomMessage` itself).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionData {
    pub command: String,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_from_context: Option<bool>,
}

/// `BranchSummaryMessage` payload. Mirrors `BranchSummaryMessage`
/// (`{summary, fromId}`, the role/timestamp live on `CustomMessage`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryData {
    pub summary: String,
    pub from_id: String,
}

/// `CompactionSummaryMessage` payload. Mirrors `CompactionSummaryMessage`
/// (`{summary, tokensBefore}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummaryData {
    pub summary: String,
    pub tokens_before: i64,
}

/// `CustomMessage<T>` payload. Mirrors `CustomMessage` (`{customType, content,
/// display, details}`) for the generic `custom` role. `content` lives on
/// `CustomMessage::content`, so the data payload carries the metadata fields
/// only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomData {
    pub custom_type: String,
    #[serde(default = "default_true")]
    pub display: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Constructors — mirror createBashExecutionMessage / createBranchSummaryMessage
// / createCompactionSummaryMessage / createCustomMessage
// ---------------------------------------------------------------------------

/// Build a `bashExecution` custom message. Mirrors the TS constructor; `content`
/// is empty (rendered from `data` at convert time, matches TS where
/// `convertToLlm` calls `bashExecutionToText`).
pub fn bash_execution_message(
    command: impl Into<String>,
    output: impl Into<String>,
    exit_code: Option<i32>,
    cancelled: bool,
    truncated: bool,
    full_output_path: Option<String>,
    exclude_from_context: Option<bool>,
    timestamp: i64,
) -> AgentMessage {
    let data = serde_json::to_value(BashExecutionData {
        command: command.into(),
        output: output.into(),
        exit_code,
        cancelled,
        truncated,
        full_output_path,
        exclude_from_context,
    })
    .expect("BashExecutionData serializes");
    AgentMessage::Custom(CustomMessage {
        role: BASH_EXECUTION_ROLE.into(),
        content: Vec::new(),
        data,
        timestamp,
    })
}

/// `createBranchSummaryMessage(summary, fromId, timestamp)`. Mirrors the TS
/// constructor; `content` is empty (rendered from `data.summary` at convert
/// time).
pub fn create_branch_summary_message(
    summary: impl Into<String>,
    from_id: impl Into<String>,
    timestamp: i64,
) -> AgentMessage {
    let data = serde_json::to_value(BranchSummaryData {
        summary: summary.into(),
        from_id: from_id.into(),
    })
    .expect("BranchSummaryData serializes");
    AgentMessage::Custom(CustomMessage {
        role: BRANCH_SUMMARY_ROLE.into(),
        content: Vec::new(),
        data,
        timestamp,
    })
}

/// `createCompactionSummaryMessage(summary, tokensBefore, timestamp)`. Mirrors
/// the TS constructor; `content` is empty (rendered from `data.summary` at
/// convert time).
pub fn create_compaction_summary_message(
    summary: impl Into<String>,
    tokens_before: i64,
    timestamp: i64,
) -> AgentMessage {
    let data = serde_json::to_value(CompactionSummaryData {
        summary: summary.into(),
        tokens_before,
    })
    .expect("CompactionSummaryData serializes");
    AgentMessage::Custom(CustomMessage {
        role: COMPACTION_SUMMARY_ROLE.into(),
        content: Vec::new(),
        data,
        timestamp,
    })
}

/// `createCustomMessage(customType, content, display, details, timestamp)`.
/// Mirrors the TS constructor; `content` (string or text/image blocks) is
/// normalized to `Vec<Content>` on the stored `CustomMessage`.
pub fn create_custom_message(
    custom_type: impl Into<String>,
    content: UserContent,
    display: bool,
    details: Option<serde_json::Value>,
    timestamp: i64,
) -> AgentMessage {
    let blocks = match content {
        UserContent::Text(s) => vec![Content::text(s)],
        UserContent::Blocks(b) => b,
    };
    let data = serde_json::to_value(CustomData {
        custom_type: custom_type.into(),
        display,
        details,
    })
    .expect("CustomData serializes");
    AgentMessage::Custom(CustomMessage {
        role: CUSTOM_ROLE.into(),
        content: blocks,
        data,
        timestamp,
    })
}

// ---------------------------------------------------------------------------
// Role-data accessors (used by compaction's estimate_tokens / convert_to_llm)
// ---------------------------------------------------------------------------

/// If `msg` is a `compactionSummary` custom message, deserialize its `data`.
pub fn compaction_summary_data(msg: &AgentMessage) -> Option<CompactionSummaryData> {
    match msg {
        AgentMessage::Custom(c) if c.role == COMPACTION_SUMMARY_ROLE => {
            serde_json::from_value(c.data.clone()).ok()
        }
        _ => None,
    }
}

/// If `msg` is a `branchSummary` custom message, deserialize its `data`.
pub fn branch_summary_data(msg: &AgentMessage) -> Option<BranchSummaryData> {
    match msg {
        AgentMessage::Custom(c) if c.role == BRANCH_SUMMARY_ROLE => {
            serde_json::from_value(c.data.clone()).ok()
        }
        _ => None,
    }
}

/// If `msg` is a `bashExecution` custom message, deserialize its `data`.
pub fn bash_execution_data(msg: &AgentMessage) -> Option<BashExecutionData> {
    match msg {
        AgentMessage::Custom(c) if c.role == BASH_EXECUTION_ROLE => {
            serde_json::from_value(c.data.clone()).ok()
        }
        _ => None,
    }
}

/// If `msg` is a generic `custom` role message, deserialize its `data`.
pub fn custom_data(msg: &AgentMessage) -> Option<CustomData> {
    match msg {
        AgentMessage::Custom(c) if c.role == CUSTOM_ROLE => {
            serde_json::from_value(c.data.clone()).ok()
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// bashExecutionToText — mirrors the TS renderer
// ---------------------------------------------------------------------------

/// Render a `bashExecution` message to its user-visible text. Mirrors
/// `bashExecutionToText`.
pub fn bash_execution_to_text(data: &BashExecutionData) -> String {
    let mut text = format!("Ran `{}`\n", data.command);
    if !data.output.is_empty() {
        text += &format!("```\n{}\n```", data.output);
    } else {
        text += "(no output)";
    }
    if data.cancelled {
        text += "\n\n(command cancelled)";
    } else if let Some(code) = data.exit_code {
        if code != 0 {
            text += &format!("\n\nCommand exited with code {code}");
        }
    }
    if data.truncated {
        if let Some(full) = &data.full_output_path {
            text += &format!("\n\n[Output truncated. Full output: {full}]");
        }
    }
    text
}

// ---------------------------------------------------------------------------
// convertToLlm — the harness-level AgentMessage[] -> Message[] converter
// ---------------------------------------------------------------------------

/// A diagnostic summary produced while repairing a recovered context.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCallRepairReport {
    pub repaired_call_ids: Vec<String>,
}

impl ToolCallRepairReport {
    pub fn is_empty(&self) -> bool {
        self.repaired_call_ids.is_empty()
    }
}

const RECOVERY_TOOL_RESULT: &str = concat!(
    "Tool call was interrupted before its result was persisted. ",
    "The tool was not re-executed during session recovery."
);

/// Repair interrupted or interleaved tool batches in the provider context.
/// Results must match both call ID and tool name, and only until the next
/// assistant message, so a reused ID cannot hide a different invocation. The
/// durable history is unchanged and no tool is executed during this repair.
pub fn repair_orphaned_tool_calls(
    messages: Vec<AgentMessage>,
) -> (Vec<AgentMessage>, ToolCallRepairReport) {
    if tool_batches_need_no_repair(&messages) {
        return (messages, ToolCallRepairReport::default());
    }
    repair_orphaned_tool_calls_slow(messages)
}

fn tool_batches_need_no_repair(messages: &[AgentMessage]) -> bool {
    // Keep borrowed IDs, including duplicate occurrences. The slow repair
    // consumes one pending occurrence per result, rather than deduplicating IDs.
    let mut pending = Vec::new();
    for message in messages {
        match message {
            AgentMessage::Assistant(assistant) => {
                if !pending.is_empty() {
                    return false;
                }
                pending.extend(assistant.content.iter().filter_map(|block| match block {
                    Content::ToolCall(call) => Some((call.id.as_str(), call.name.as_str())),
                    _ => None,
                }));
            }
            AgentMessage::ToolResult(result) => {
                let Some(index) = pending
                    .iter()
                    .position(|(id, name)| *id == result.tool_call_id && *name == result.tool_name)
                else {
                    return false;
                };
                pending.remove(index);
            }
            _ if !pending.is_empty() => return false,
            _ => {}
        }
    }
    pending.is_empty()
}

fn repair_orphaned_tool_calls_slow(
    messages: Vec<AgentMessage>,
) -> (Vec<AgentMessage>, ToolCallRepairReport) {
    let mut input = messages.into_iter().peekable();
    let mut repaired = Vec::new();
    let mut report = ToolCallRepairReport::default();
    while let Some(message) = input.next() {
        let AgentMessage::Assistant(assistant) = &message else {
            if let AgentMessage::ToolResult(result) = &message {
                tracing::warn!(call_id = %result.tool_call_id, "omitting an orphaned tool result from provider context");
            } else {
                repaired.push(message);
            }
            continue;
        };
        let timestamp = assistant.timestamp;
        let mut pending: Vec<(String, String)> = assistant
            .content
            .iter()
            .filter_map(|content| match content {
                Content::ToolCall(call) => Some((call.id.clone(), call.name.clone())),
                _ => None,
            })
            .collect();
        repaired.push(message);
        let mut interleaved = Vec::new();
        // Move ownership of the messages; images and large tool outputs are not
        // cloned just to adjust their position.
        while input
            .peek()
            .is_some_and(|message| !matches!(message, AgentMessage::Assistant(_)))
        {
            let message = input.next().expect("peeked message");
            if let AgentMessage::ToolResult(result) = &message {
                if let Some(index) = pending
                    .iter()
                    .position(|(id, name)| id == &result.tool_call_id && name == &result.tool_name)
                {
                    pending.remove(index);
                    repaired.push(message);
                } else {
                    tracing::warn!(call_id = %result.tool_call_id, "omitting an unmatched or duplicate tool result from provider context");
                }
            } else {
                interleaved.push(message);
            }
        }
        for (tool_call_id, tool_name) in pending {
            report.repaired_call_ids.push(tool_call_id.clone());
            repaired.push(AgentMessage::ToolResult(Box::new(ToolResultMessage {
                role: ToolResultRole,
                tool_call_id,
                tool_name,
                content: vec![Content::text(RECOVERY_TOOL_RESULT)],
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                is_error: true,
                timestamp,
            })));
        }
        repaired.extend(interleaved);
    }
    (repaired, report)
}

/// The harness converter. It moves tool results next to their assistant and
/// repairs interrupted tool calls, then mirrors TS `convertToLlm`: renders
/// custom roles as user text and passes through normal user/assistant/tool-result
/// messages.
pub fn convert_to_llm(messages: Vec<AgentMessage>) -> Vec<Message> {
    let (messages, report) = repair_orphaned_tool_calls(messages);
    if !report.repaired_call_ids.is_empty() {
        tracing::warn!(
            repaired = report.repaired_call_ids.len(),
            call_ids = ?report.repaired_call_ids,
            "repaired orphaned tool calls while building provider context"
        );
    }
    messages
        .into_iter()
        .filter_map(|m| match m {
            AgentMessage::User(u) => Some(Message::User(u)),
            AgentMessage::Assistant(a) => Some(Message::Assistant(a)),
            AgentMessage::ToolResult(t) => Some(Message::ToolResult(t)),
            AgentMessage::Custom(c) => convert_custom_to_llm(c),
        })
        .collect()
}

/// Convert a suffix after the initial history has already gone through repair.
/// New loop-produced messages are appended in valid provider order, so no
/// cross-boundary orphan repair is needed here.
pub fn convert_to_llm_incremental(messages: &[AgentMessage]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|message| match message {
            AgentMessage::User(user) => Some(Message::User(user.clone())),
            AgentMessage::Assistant(assistant) => Some(Message::Assistant(assistant.clone())),
            AgentMessage::ToolResult(tool_result) => Some(Message::ToolResult(tool_result.clone())),
            AgentMessage::Custom(custom) => convert_custom_to_llm(custom.clone()),
        })
        .collect()
}
fn convert_custom_to_llm(c: CustomMessage) -> Option<Message> {
    let timestamp = c.timestamp;
    match c.role.as_str() {
        BASH_EXECUTION_ROLE => {
            let data: BashExecutionData = serde_json::from_value(c.data).ok()?;
            if data.exclude_from_context.unwrap_or(false) {
                return None;
            }
            let text = bash_execution_to_text(&data);
            Some(Message::User(UserMessage::new(text, timestamp)))
        }
        CUSTOM_ROLE => {
            // Generic custom: content is the stored text/image blocks.
            Some(Message::User(UserMessage::new(
                UserContent::Blocks(c.content),
                timestamp,
            )))
        }
        BRANCH_SUMMARY_ROLE => {
            let data: BranchSummaryData = serde_json::from_value(c.data).ok()?;
            let text = format!(
                "{BRANCH_SUMMARY_PREFIX}{}{BRANCH_SUMMARY_SUFFIX}",
                data.summary
            );
            Some(Message::User(UserMessage::new(text, timestamp)))
        }
        COMPACTION_SUMMARY_ROLE => {
            let data: CompactionSummaryData = serde_json::from_value(c.data).ok()?;
            let text = format!(
                "{COMPACTION_SUMMARY_PREFIX}{}{COMPACTION_SUMMARY_SUFFIX}",
                data.summary
            );
            Some(Message::User(UserMessage::new(text, timestamp)))
        }
        // Unregistered custom role → drop (mirrors TS `default → undefined`).
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ai::types::{Api, AssistantMessage, ToolCall, ToolCallType};

    fn assistant_with_calls(ids: &[(&str, &str)]) -> AgentMessage {
        let mut assistant = AssistantMessage::empty(Api::OpenaiCompletions, "test", "model", 42);
        assistant.content = ids
            .iter()
            .map(|(id, name)| {
                Content::ToolCall(ToolCall {
                    kind: ToolCallType,
                    id: (*id).to_string(),
                    name: (*name).to_string(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                    namespace: None,
                    recorded: None,
                })
            })
            .collect();
        AgentMessage::Assistant(Box::new(assistant))
    }

    fn tool_result(id: &str, name: &str) -> AgentMessage {
        AgentMessage::ToolResult(Box::new(ToolResultMessage {
            role: ToolResultRole,
            tool_call_id: id.to_string(),
            tool_name: name.to_string(),
            content: vec![Content::text("ok")],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: 43,
        }))
    }

    #[test]
    fn settled_tool_batches_reuse_the_owned_history_list() {
        let messages = vec![
            AgentMessage::User(UserMessage::new("prompt", 0)),
            assistant_with_calls(&[("a", "read"), ("b", "write")]),
            tool_result("b", "write"),
            tool_result("a", "read"),
            AgentMessage::User(UserMessage::new("next", 0)),
        ];
        let allocation = messages.as_ptr();
        let (repaired, report) = repair_orphaned_tool_calls(messages);
        assert!(report.is_empty());
        assert_eq!(
            repaired.as_ptr(),
            allocation,
            "a complete ordered batch needs no rebuilding"
        );
    }

    #[test]
    fn repair_fast_path_matches_original_for_all_small_batch_combinations() {
        let catalog = [
            AgentMessage::User(UserMessage::new("prompt", 0)),
            create_custom_message("note", UserContent::Text("note".into()), true, None, 0),
            assistant_with_calls(&[]),
            assistant_with_calls(&[("a", "read")]),
            assistant_with_calls(&[("a", "read"), ("b", "write")]),
            assistant_with_calls(&[("a", "read"), ("a", "write")]),
            tool_result("a", "read"),
            tool_result("b", "write"),
        ];
        for length in 0..=4 {
            for mut code in 0..catalog.len().pow(length) {
                let mut messages = Vec::new();
                for _ in 0..length {
                    messages.push(catalog[code % catalog.len()].clone());
                    code /= catalog.len();
                }
                let original = repair_orphaned_tool_calls_slow(messages.clone());
                assert_eq!(repair_orphaned_tool_calls(messages), original);
            }
        }
    }

    #[test]
    fn repair_inserts_error_result_for_missing_tool_output() {
        let original = assistant_with_calls(&[("call-1", "read")]);
        let (repaired, report) = repair_orphaned_tool_calls(vec![original.clone()]);

        assert_eq!(report.repaired_call_ids, vec!["call-1"]);
        assert_eq!(repaired.len(), 2);
        assert_eq!(repaired[0], original);
        let AgentMessage::ToolResult(result) = &repaired[1] else {
            panic!("expected synthetic tool result");
        };
        assert_eq!(result.tool_call_id, "call-1");
        assert_eq!(result.tool_name, "read");
        assert!(result.is_error);
        assert!(
            matches!(&result.content[0], Content::Text(text) if text.text.contains("not re-executed"))
        );
    }

    #[test]
    fn repair_handles_parallel_calls_and_preserves_existing_results() {
        let assistant = assistant_with_calls(&[("call-1", "read"), ("call-2", "bash")]);
        let (repaired, report) =
            repair_orphaned_tool_calls(vec![assistant, tool_result("call-1", "read")]);

        assert_eq!(report.repaired_call_ids, vec!["call-2"]);
        assert_eq!(repaired.len(), 3);
        assert!(
            matches!(&repaired[1], AgentMessage::ToolResult(result) if result.tool_call_id == "call-1" && !result.is_error)
        );
        assert!(
            matches!(&repaired[2], AgentMessage::ToolResult(result) if result.tool_call_id == "call-2" && result.is_error)
        );
    }

    #[test]
    fn normal_history_is_not_changed_by_repair() {
        let messages = vec![
            assistant_with_calls(&[("call-1", "read")]),
            tool_result("call-1", "read"),
        ];
        let (repaired, report) = repair_orphaned_tool_calls(messages.clone());
        assert!(report.is_empty());
        assert_eq!(repaired, messages);
    }

    #[test]
    fn repair_keeps_parallel_results_and_missing_results_before_interleaved_context() {
        let context = AgentMessage::User(UserMessage::new("injected", 0));
        let (out, report) = repair_orphaned_tool_calls(vec![
            assistant_with_calls(&[("a", "read"), ("b", "write"), ("c", "bash")]),
            context.clone(),
            tool_result("b", "write"),
            tool_result("a", "read"),
        ]);
        assert_eq!(report.repaired_call_ids, vec!["c"]);
        assert!(
            matches!(&out[1], AgentMessage::ToolResult(t) if t.tool_call_id == "b" && !t.is_error)
        );
        assert!(
            matches!(&out[2], AgentMessage::ToolResult(t) if t.tool_call_id == "a" && !t.is_error)
        );
        assert!(
            matches!(&out[3], AgentMessage::ToolResult(t) if t.tool_call_id == "c" && t.is_error)
        );
        assert_eq!(out[4], context);
        let (again, report) = repair_orphaned_tool_calls(out.clone());
        assert_eq!(again, out);
        assert!(report.is_empty());
    }

    #[test]
    fn repair_does_not_steal_a_reused_id_from_a_later_assistant() {
        let (out, report) = repair_orphaned_tool_calls(vec![
            assistant_with_calls(&[("same", "read")]),
            AgentMessage::User(UserMessage::new("next turn", 0)),
            assistant_with_calls(&[("same", "write")]),
            tool_result("same", "write"),
        ]);
        assert_eq!(report.repaired_call_ids, vec!["same"]);
        assert!(
            matches!(&out[1], AgentMessage::ToolResult(t) if t.tool_name == "read" && t.is_error)
        );
        assert!(
            matches!(&out[4], AgentMessage::ToolResult(t) if t.tool_name == "write" && !t.is_error)
        );
    }

    #[test]
    fn repair_preserves_valid_reused_ids_and_omits_duplicate_or_backward_results() {
        let history = vec![
            assistant_with_calls(&[("same", "read")]),
            tool_result("same", "read"),
            assistant_with_calls(&[("same", "write")]),
            tool_result("same", "write"),
        ];
        let (out, report) = repair_orphaned_tool_calls(history.clone());
        assert_eq!(out, history);
        assert!(report.is_empty());
        let (out, report) = repair_orphaned_tool_calls(vec![
            tool_result("a", "read"),
            assistant_with_calls(&[("a", "read")]),
            tool_result("a", "read"),
            tool_result("a", "read"),
        ]);
        assert_eq!(out.len(), 2);
        assert!(report.is_empty());
    }

    #[test]
    fn repair_moves_image_content_without_copying_its_payload() {
        let image = rpi_ai::types::ImageContent {
            kind: rpi_ai::types::ImageContentType,
            data: "large image".repeat(1024).into(),
            mime_type: "image/png".into(),
        };
        let original_pointer = image.data.as_ptr();
        let (out, _) = repair_orphaned_tool_calls(vec![
            assistant_with_calls(&[("a", "read")]),
            AgentMessage::User(UserMessage::new(
                UserContent::Blocks(vec![Content::Image(image)]),
                0,
            )),
            tool_result("a", "read"),
        ]);
        let AgentMessage::User(user) = &out[2] else {
            panic!("user")
        };
        let UserContent::Blocks(content) = &user.content else {
            panic!("blocks")
        };
        let Content::Image(image) = &content[0] else {
            panic!("image")
        };
        assert_eq!(image.data.as_ptr(), original_pointer);
    }

    #[test]
    fn compaction_summary_round_trips_data() {
        let m = create_compaction_summary_message("sum", 42, 100);
        let d = compaction_summary_data(&m).unwrap();
        assert_eq!(d.summary, "sum");
        assert_eq!(d.tokens_before, 42);
    }

    #[test]
    fn branch_summary_round_trips_data() {
        let m = create_branch_summary_message("s", "from-1", 7);
        let d = branch_summary_data(&m).unwrap();
        assert_eq!(d.summary, "s");
        assert_eq!(d.from_id, "from-1");
    }

    #[test]
    fn convert_to_llm_renders_compaction_summary_as_user_text() {
        let m = create_compaction_summary_message("hello", 10, 1);
        let out = convert_to_llm(vec![m]);
        assert_eq!(out.len(), 1);
        match &out[0] {
            Message::User(u) => match &u.content {
                UserContent::Text(s) => {
                    assert!(s.contains("hello"));
                    assert!(s.contains("<summary>"));
                }
                _ => panic!("expected text content"),
            },
            _ => panic!("expected user message"),
        }
    }

    #[test]
    fn convert_to_llm_drops_bash_when_excluded() {
        let m = bash_execution_message("ls", "out", None, false, false, None, Some(true), 1);
        assert!(convert_to_llm(vec![m]).is_empty());
    }

    #[test]
    fn convert_to_llm_renders_bash_execution_to_text() {
        let m = bash_execution_message("echo hi", "hi", Some(0), false, false, None, None, 1);
        let out = convert_to_llm(vec![m]);
        assert_eq!(out.len(), 1);
        match &out[0] {
            Message::User(u) => match &u.content {
                UserContent::Text(s) => {
                    assert!(s.contains("Ran `echo hi`"));
                    assert!(s.contains("hi"));
                }
                _ => panic!("expected text"),
            },
            _ => panic!("expected user"),
        }
    }

    #[test]
    fn convert_to_llm_passes_through_base_roles() {
        let u = AgentMessage::User(UserMessage::new("hi", 1));
        let out = convert_to_llm(vec![u]);
        assert!(matches!(out[0], Message::User(_)));
    }

    #[test]
    fn incremental_conversion_matches_full_conversion_for_valid_suffixes() {
        let messages = vec![
            AgentMessage::User(UserMessage::new("hi", 1)),
            create_compaction_summary_message("summary", 2, 3),
            assistant_with_calls(&[("call_a", "goal")]),
            tool_result("call_a", "goal"),
        ];
        assert_eq!(
            convert_to_llm(messages.clone()),
            convert_to_llm_incremental(&messages)
        );
    }
    #[test]
    fn convert_to_llm_drops_unregistered_custom_role() {
        let c = CustomMessage::new("weird".to_string(), Vec::new(), serde_json::Value::Null, 1);
        let out = convert_to_llm(vec![AgentMessage::Custom(c)]);
        assert!(out.is_empty());
    }

    #[test]
    fn convert_to_llm_moves_an_interleaved_message_after_the_tool_results() {
        // A context injection appended mid tool-batch lands between the
        // assistant's tool_calls and its result. It must be moved after the
        // result, or strict providers reject the request with HTTP 400.
        let custom = CustomMessage::new(
            CUSTOM_ROLE.to_string(),
            vec![Content::text("injected")],
            serde_json::Value::Null,
            1,
        );
        let messages = vec![
            assistant_with_calls(&[("call_a", "goal")]),
            AgentMessage::Custom(custom),
            tool_result("call_a", "goal"),
        ];
        let out = convert_to_llm(messages);
        assert_eq!(out.len(), 3);
        match (&out[0], &out[1], &out[2]) {
            (Message::Assistant(a), Message::ToolResult(t), Message::User(_)) => {
                assert!(a
                    .content
                    .iter()
                    .any(|c| matches!(c, Content::ToolCall(tc) if tc.id == "call_a")));
                assert_eq!(t.tool_call_id, "call_a");
            }
            _ => panic!("assistant tool_calls must be immediately followed by its tool result"),
        }
    }
}
