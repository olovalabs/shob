//! Mirrors the token-estimation + file-operation + conversation-serialization
//! pieces of `packages/agent/src/harness/compaction/utils.ts`, plus
//! `estimate_tokens`/`estimate_context_tokens` (from `compaction.ts`).
//!
//! The context-builder pieces (`build_session_context`/
//! `default_context_entry_transform`/`SessionContext`) are re-exported from
//! the canonical `session::context` module (the authoritative
//! `session/context.ts` port) — compaction's `tokens_before` path delegates to
//! that module's no-options wrapper so the two never diverge.

use std::collections::BTreeSet;

use rpi_agent::message::AgentMessage;
use rpi_ai::types::{AssistantMessage, Content, Message, Usage};

use crate::messages::{bash_execution_data, branch_summary_data, compaction_summary_data};
use crate::session::types::Entry;

// ---------------------------------------------------------------------------
// File operations — mirrors utils.ts FileOperations / extractFileOpsFromMessage
// / computeFileLists / formatFileOperations
// ---------------------------------------------------------------------------

/// Files touched by a session branch or compaction range. Mirrors TS
/// `FileOperations`. `BTreeSet` (not `HashSet`) so derived lists are sorted and
/// deterministic — the TS code sorts at `computeFileLists` time; we keep the
/// sets unsorted and sort once at list-computation, matching that.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileOperations {
    pub read: BTreeSet<String>,
    pub written: BTreeSet<String>,
    pub edited: BTreeSet<String>,
}

/// `createFileOps()` — an empty accumulator.
pub fn create_file_ops() -> FileOperations {
    FileOperations::default()
}

/// Add file operations from an assistant message's tool calls. Mirrors
/// `extractFileOpsFromMessage`: only `assistant` messages, only `toolCall`
/// blocks, classified by tool `name` (`read`/`write`/`edit`) using
/// `arguments.path`.
pub fn extract_file_ops_from_message(message: &AgentMessage, file_ops: &mut FileOperations) {
    let assistant = match message {
        AgentMessage::Assistant(a) => a,
        _ => return,
    };
    for block in &assistant.content {
        if let Content::ToolCall(call) = block {
            let path = match call.arguments.get("path") {
                Some(v) => match v.as_str() {
                    Some(s) => s,
                    None => continue,
                },
                None => continue,
            };
            match call.name.as_str() {
                "read" => {
                    file_ops.read.insert(path.to_string());
                }
                "write" => {
                    file_ops.written.insert(path.to_string());
                }
                "edit" => {
                    file_ops.edited.insert(path.to_string());
                }
                _ => {}
            }
        }
    }
}

/// Compute sorted read-only and modified file lists from accumulated operations.
/// Mirrors `computeFileLists`: `modified = edited ∪ written`; `readOnly = read
/// − modified`.
pub fn compute_file_lists(file_ops: &FileOperations) -> (Vec<String>, Vec<String>) {
    let modified: BTreeSet<String> = file_ops.edited.union(&file_ops.written).cloned().collect();
    let read_only: Vec<String> = file_ops
        .read
        .iter()
        .filter(|f| !modified.contains(*f))
        .cloned()
        .collect();
    let modified_files: Vec<String> = modified.into_iter().collect();
    (read_only, modified_files)
}

/// Format file lists as summary metadata XML tags. Mirrors
/// `formatFileOperations`: empty input → `""`; non-empty → `"\n\n"` + sections
/// joined `"\n\n"`.
pub fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!(
            "<read-files>\n{}\n</read-files>",
            read_files.join("\n")
        ));
    }
    if !modified_files.is_empty() {
        sections.push(format!(
            "<modified-files>\n{}\n</modified-files>",
            modified_files.join("\n")
        ));
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", sections.join("\n\n"))
    }
}

// ---------------------------------------------------------------------------
// serializeConversation — mirrors utils.ts (operates on provider Message[])
// ---------------------------------------------------------------------------

const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// `safeJsonStringify` — `JSON.stringify` with a fallback. Rust's
/// `serde_json::to_string` can fail on non-finite floats / cycles (cycles
/// impossible for `Value`); mirror the fallback string.
fn safe_json_stringify(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "[unserializable]".to_string())
}

fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    // TS slices by UTF-16 code units; for ASCII-heavy tool output this matches.
    // For multibyte content we slice by char boundary to stay valid UTF-8.
    let truncated: String = text.chars().take(max_chars).collect();
    let more = text.chars().count().saturating_sub(max_chars);
    format!("{truncated}\n\n[... {more} more characters truncated]")
}

/// Serialize LLM messages to plain text for summarization prompts. Mirrors
/// `serializeConversation`: `[User]`/`[Assistant]`/`[Assistant thinking]`/
/// `[Assistant tool calls]`/`[Tool result]` blocks joined `\n\n`.
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for msg in messages {
        match msg {
            Message::User(u) => {
                let content = user_content_text(&u.content);
                if !content.is_empty() {
                    parts.push(format!("[User]: {content}"));
                }
            }
            Message::Assistant(a) => {
                let mut thinking_parts: Vec<String> = Vec::new();
                let mut tool_calls: Vec<String> = Vec::new();
                for block in &a.content {
                    match block {
                        Content::Thinking(t) => thinking_parts.push(t.thinking.clone()),
                        Content::ToolCall(call) => {
                            let args_str = match &call.arguments {
                                serde_json::Value::Object(map) => map
                                    .iter()
                                    .map(|(k, v)| format!("{k}={}", safe_json_stringify(v)))
                                    .collect::<Vec<_>>()
                                    .join(", "),
                                other => safe_json_stringify(other),
                            };
                            tool_calls.push(format!("{}({})", call.name, args_str));
                        }
                        _ => {}
                    }
                }
                if !thinking_parts.is_empty() {
                    parts.push(format!(
                        "[Assistant thinking]: {}",
                        thinking_parts.join("\n")
                    ));
                }
                if a.content.iter().any(|b| matches!(b, Content::Text(_))) {
                    parts.push(format!(
                        "[Assistant]: {}",
                        Content::text_only(&a.content, "\n")
                    ));
                }
                if !tool_calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", tool_calls.join("; ")));
                }
            }
            Message::ToolResult(t) => {
                let content = Content::text_only(&t.content, "");
                if !content.is_empty() {
                    parts.push(format!(
                        "[Tool result]: {}",
                        truncate_for_summary(&content, TOOL_RESULT_MAX_CHARS)
                    ));
                }
            }
        }
    }
    parts.join("\n\n")
}

fn user_content_text(content: &rpi_ai::types::UserContent) -> String {
    match content {
        rpi_ai::types::UserContent::Text(s) => s.clone(),
        rpi_ai::types::UserContent::Blocks(blocks) => Content::text_only(blocks, ""),
    }
}

// ---------------------------------------------------------------------------
// estimateTokens / estimateContextTokens — mirrors compaction.ts
// ---------------------------------------------------------------------------

const ESTIMATED_IMAGE_CHARS: usize = 4800;

/// Char count for text+image content (string or content blocks). Mirrors
/// `estimateTextAndImageContentChars`.
fn estimate_text_and_image_chars_content(content: &[Content]) -> usize {
    let mut chars = 0;
    for block in content {
        match block {
            Content::Text(t) => chars += t.text.len(),
            Content::Image(_) => chars += ESTIMATED_IMAGE_CHARS,
            _ => {}
        }
    }
    chars
}

/// Estimate token count for one message using the conservative chars/4
/// heuristic. Mirrors `estimateTokens(message)` per role.
pub fn estimate_tokens(message: &AgentMessage) -> i64 {
    let chars = estimate_message_chars(message);
    div_ceil(chars as i64, 4)
}

fn estimate_message_chars(message: &AgentMessage) -> usize {
    match message {
        AgentMessage::User(u) => match &u.content {
            rpi_ai::types::UserContent::Text(s) => s.len(),
            rpi_ai::types::UserContent::Blocks(blocks) => {
                estimate_text_and_image_chars_content(blocks)
            }
        },
        AgentMessage::Assistant(a) => estimate_assistant_chars(a),
        AgentMessage::ToolResult(t) => estimate_text_and_image_chars_content(&t.content),
        AgentMessage::Custom(c) => estimate_custom_chars(c),
    }
}

fn estimate_assistant_chars(a: &AssistantMessage) -> usize {
    let mut chars = 0;
    for block in &a.content {
        match block {
            Content::Text(t) => chars += t.text.len(),
            Content::Thinking(t) => chars += t.thinking.len(),
            Content::ToolCall(call) => {
                chars += call.name.len() + safe_json_stringify(&call.arguments).len();
            }
            _ => {}
        }
    }
    chars
}

fn estimate_custom_chars(c: &rpi_agent::message::CustomMessage) -> usize {
    match c.role.as_str() {
        crate::messages::BASH_EXECUTION_ROLE => {
            if let Some(d) = bash_execution_data(&AgentMessage::Custom(c.clone())) {
                d.command.len() + d.output.len()
            } else {
                0
            }
        }
        crate::messages::BRANCH_SUMMARY_ROLE | crate::messages::COMPACTION_SUMMARY_ROLE => {
            if let Some(d) = branch_summary_data(&AgentMessage::Custom(c.clone())) {
                d.summary.len()
            } else if let Some(d) = compaction_summary_data(&AgentMessage::Custom(c.clone())) {
                d.summary.len()
            } else {
                0
            }
        }
        // Generic `custom` + unregistered roles → text+image chars of stored
        // content (mirrors the TS `custom`/`toolResult` arm).
        _ => estimate_text_and_image_chars_content(&c.content),
    }
}

fn div_ceil(a: i64, b: i64) -> i64 {
    // `Math.ceil(chars / 4)` — integer ceil division. Written by hand (not
    // `i64::div_ceil`) to keep the crate's MSRV (1.78) happy; `div_ceil`
    // stabilized only in 1.87.
    (a + b - 1) / b
}

/// Return the last valid assistant usage (stop_reason not aborted/error,
/// `context_tokens > 0`) along with its index, or `None`. Mirrors
/// `getLastAssistantUsageInfo`.
fn last_assistant_usage_info(messages: &[AgentMessage]) -> Option<(Usage, usize)> {
    for (i, msg) in messages.iter().enumerate().rev() {
        if let AgentMessage::Assistant(a) = msg {
            if a.stop_reason != rpi_ai::types::StopReason::Aborted
                && a.stop_reason != rpi_ai::types::StopReason::Error
                && a.usage.context_tokens() > 0
            {
                return Some((a.usage.clone(), i));
            }
        }
    }
    None
}

/// Estimated context-token usage for a message list. Mirrors
/// `estimateContextTokens` + `ContextUsageEstimate`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextUsageEstimate {
    pub tokens: i64,
    pub usage_tokens: i64,
    pub trailing_tokens: i64,
    pub last_usage_index: Option<usize>,
}

pub fn estimate_context_tokens(messages: &[AgentMessage]) -> ContextUsageEstimate {
    match last_assistant_usage_info(messages) {
        None => {
            let mut estimated = 0i64;
            for m in messages {
                estimated += estimate_tokens(m);
            }
            ContextUsageEstimate {
                tokens: estimated,
                usage_tokens: 0,
                trailing_tokens: estimated,
                last_usage_index: None,
            }
        }
        Some((usage, index)) => {
            let usage_tokens = usage.context_tokens();
            let mut trailing_tokens = 0i64;
            for m in messages.iter().skip(index + 1) {
                trailing_tokens += estimate_tokens(m);
            }
            ContextUsageEstimate {
                tokens: usage_tokens + trailing_tokens,
                usage_tokens,
                trailing_tokens,
                last_usage_index: Some(index),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// build_session_context — compaction's tokensBefore path delegates to the
// canonical `session::context` module (the authoritative `session/context.ts`
// port). The no-options shape compaction needs is `build_session_context_default`.
// ---------------------------------------------------------------------------

/// `defaultContextEntryTransform` — re-exported from the canonical context
/// module so compaction callers reach it without a second path. Mirrors
/// `defaultContextEntryTransform`.
pub use crate::session::context::default_context_entry_transform;

/// `SessionContext` — re-exported from the canonical context module. Mirrors TS
/// `SessionContext`. Carried in `compaction::tokens` for tokens-path callers.
pub use crate::session::context::SessionContext;

/// `buildSessionContext(pathEntries)` (no options) — the shape compaction uses
/// for `tokensBefore`. Delegates to the canonical context builder. Mirrors TS
/// `buildSessionContext`.
pub fn build_session_context(path_entries: &[Entry]) -> SessionContext {
    crate::session::context::build_session_context(path_entries, &Default::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_agent::message::AgentMessage;
    use rpi_ai::types::{UserContent, UserMessage};

    fn user(text: &str) -> AgentMessage {
        AgentMessage::User(UserMessage::new(UserContent::Text(text.into()), 1))
    }

    #[test]
    fn estimate_tokens_user_chars_div_4_ceil() {
        // 8 chars → ceil(8/4)=2.
        assert_eq!(estimate_tokens(&user("abcdefgh")), 2);
        // 9 chars → ceil(9/4)=3.
        assert_eq!(estimate_tokens(&user("abcdefghi")), 3);
        assert_eq!(estimate_tokens(&user("")), 0);
    }

    #[test]
    fn extract_file_ops_classifies_read_write_edit() {
        // Build a real assistant with three tool calls.
        let mut a =
            rpi_ai::types::AssistantMessage::empty(rpi_ai::types::Api::Faux, "faux", "faux", 1);
        a.content.push(Content::tool_call(
            "c1",
            "read",
            serde_json::json!({"path": "/a"}),
        ));
        a.content.push(Content::tool_call(
            "c2",
            "write",
            serde_json::json!({"path": "/b"}),
        ));
        a.content.push(Content::tool_call(
            "c3",
            "edit",
            serde_json::json!({"path": "/c"}),
        ));
        let msg = AgentMessage::Assistant(Box::new(a));
        let mut ops = create_file_ops();
        extract_file_ops_from_message(&msg, &mut ops);
        assert!(ops.read.contains("/a"));
        assert!(ops.written.contains("/b"));
        assert!(ops.edited.contains("/c"));
        let (read, modified) = compute_file_lists(&ops);
        assert_eq!(read, vec!["/a".to_string()]);
        assert_eq!(modified, vec!["/b".to_string(), "/c".to_string()]);
    }

    #[test]
    fn format_file_operations_empty_and_nonempty() {
        assert_eq!(format_file_operations(&[], &[]), "");
        let s = format_file_operations(&["/a".into(), "/b".into()], &["/c".into()]);
        assert!(s.starts_with("\n\n"));
        assert!(s.contains("<read-files>"));
        assert!(s.contains("/a"));
        assert!(s.contains("<modified-files>"));
        assert!(s.contains("/c"));
    }

    #[test]
    fn serialize_conversation_renders_roles() {
        let msgs = vec![Message::User(UserMessage::new(
            UserContent::Text("hello".into()),
            1,
        ))];
        let s = serialize_conversation(&msgs);
        assert_eq!(s, "[User]: hello");
    }

    #[test]
    fn estimate_context_tokens_no_usage_sums_all() {
        let msgs = vec![user("abcdefgh"), user("abcdefgh")]; // 2 + 2 = 4.
        let est = estimate_context_tokens(&msgs);
        assert_eq!(est.tokens, 4);
        assert_eq!(est.usage_tokens, 0);
        assert!(est.last_usage_index.is_none());
    }

    #[test]
    fn estimate_context_tokens_uses_last_assistant_usage_plus_trailing() {
        let mut a =
            rpi_ai::types::AssistantMessage::empty(rpi_ai::types::Api::Faux, "faux", "faux", 1);
        a.usage = Usage {
            input: 100,
            output: 10,
            cache_read: 0,
            cache_write: 0,
            cache_write_1h: None,
            reasoning: None,
            total_tokens: 110,
            cost: rpi_ai::types::UsageCost::default(),
        };
        a.stop_reason = rpi_ai::types::StopReason::Stop;
        let msgs = vec![
            user("abcdefgh"),                     // 2
            AgentMessage::Assistant(Box::new(a)), // usage 110, index 1
            user("abcdefghi"),                    // 3 trailing
        ];
        let est = estimate_context_tokens(&msgs);
        assert_eq!(est.usage_tokens, 110);
        assert_eq!(est.trailing_tokens, 3);
        assert_eq!(est.tokens, 113);
        assert_eq!(est.last_usage_index, Some(1));
    }
}
