//! Mirrors `packages/agent/src/harness/compaction/branch-summarization.ts` —
//! summarize an abandoned branch tail into a `branchSummary` entry when the
//! conversation navigates away from it.
//!
//! `collect_entries_for_branch_summary` walks a [`SessionTree`] (the Rust
//! equivalent of TS `Session`); `prepare_branch_entries` + `generate_branch_sum
//! mary` are pure/IO-bounded over the collected entries. The generated summary
//! reuses [`complete_simple_with_ret  ries`] + the compaction prompts from
//! [`crate::compaction::compaction`].

use std::collections::BTreeSet;

use rpi_agent::message::AgentMessage;
use rpi_ai::types::{Content, Context, Message, StopReason, Usage, UserContent, UserMessage};
use rpi_ai::ThinkingLevel;
use serde::{Deserialize, Serialize};

use crate::compaction::compaction::{
    complete_simple_with_retries, now_ms, CompactionLlmOptions, SUMMARIZATION_SYSTEM_PROMPT,
};
use crate::compaction::tokens::{
    compute_file_lists, create_file_ops, estimate_tokens, extract_file_ops_from_message,
    format_file_operations, serialize_conversation, FileOperations,
};
use crate::error::SessionResult;
use crate::messages::convert_to_llm;
use crate::messages::{create_branch_summary_message, create_compaction_summary_message};
use crate::session::types::{BranchBounds, Entry, EntryOrder, EntryQuery, SessionTree};

// Re-export so callers can reach the branch-summary error type via
// `compaction::branch_summary`.
pub use crate::compaction::compaction::CompactionError as BranchSummaryError;

/// Generated branch summary data ready to be persisted. Mirrors TS
/// `BranchSummaryResult`.
#[derive(Debug, Clone, PartialEq)]
pub struct BranchSummaryResult {
    pub summary: String,
    pub usage: Option<Usage>,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// File-operation details stored on generated branch summary entries. Mirrors
/// TS `BranchSummaryDetails`. Serialized so it can round-trip on a
/// `BranchSummaryEntry::details: Option<JsonValue>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummaryDetails {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

/// Prepared branch content for summarization. Mirrors `BranchPreparation`.
#[derive(Debug, Clone, PartialEq)]
pub struct BranchPreparation {
    pub messages: Vec<AgentMessage>,
    pub file_ops: FileOperations,
    pub total_tokens: i64,
}

/// Entries selected for branch summarization. Mirrors `CollectEntriesResult`.
#[derive(Debug, Clone, PartialEq)]
pub struct CollectEntriesResult {
    pub entries: Vec<Entry>,
    pub common_ancestor_id: Option<String>,
}

/// Options for generating a branch summary. Mirrors
/// `GenerateBranchSummaryOptions`; the `models`/`model`/`signal`/`retry` fields
/// consolidate into [`CompactionLlmOptions`], and `reserveTokens` defaults to
/// the compaction default.
#[derive(Clone)]
pub struct GenerateBranchSummaryOptions {
    pub llm: CompactionLlmOptions,
    /// Replace the default prompt with `custom_instructions` instead of
    /// appending. Mirrors TS `replaceInstructions`.
    pub replace_instructions: bool,
    /// Tokens reserved for prompt + model output. Defaults to 16384.
    pub reserve_tokens: i64,
}

impl GenerateBranchSummaryOptions {
    /// Default reserve tokens (mirrors TS `reserveTokens = 16384`).
    pub const DEFAULT_RESERVE_TOKENS: i64 = 16384;
}

impl Default for GenerateBranchSummaryOptions {
    fn default() -> Self {
        // `reserve_tokens` is resolved at call time; `replace_instructions`
        // defaults false. `llm` has no default — callers must populate it, so
        // this Default is a convenience that panics-free leaves `llm` to be
        // built explicitly. We do NOT impl a usable Default for `CompactionLlm
        // Options` (it has no sensible default), so this impl is intentionally
        // minimal — tests/callers construct `GenerateBranchSummaryOptions`
        // struct-literal with a real `llm`.
        unreachable!("GenerateBranchSummaryOptions must be constructed struct-literal with `llm`")
    }
}

/// `getMessageFromEntry` for branch summarization. Mirrors the local helper in
/// `branch-summarization.ts`: drops `toolResult` messages (their context is
/// covered by the assistant tool call); renders `branch_summary`/`compaction`
/// entries as their summary messages; drops metadata-only entries.
fn get_message_from_entry(entry: &Entry) -> Option<AgentMessage> {
    match entry {
        Entry::Message(m) => {
            if matches!(
                m.message.role(),
                rpi_agent::message::AgentMessageRole::ToolResult
            ) {
                return None;
            }
            Some(m.message.clone())
        }
        Entry::BranchSummary(b) => Some(create_branch_summary_message(
            &b.summary,
            &b.from_id,
            b.base.timestamp,
        )),
        Entry::Compaction(c) => Some(create_compaction_summary_message(
            &c.summary,
            c.tokens_before,
            c.base.timestamp,
        )),
        // thinking_level_change / model_change / active_tools_change / custom.
        _ => None,
    }
}

/// Collect entries that should be summarized before navigating to a different
/// session tree entry. Mirrors `collectEntriesForBranchSummary`.
///
/// Walks `old_leaf_id` back toward the common ancestor with `target_id`'s path
/// (exclusive of the ancestor), reversed into chronological order. Returns
/// empty when `old_leaf_id` is `None`. Archival tool-program body entries are
/// excluded from the collected payloads, not from common-ancestor identity.
/// Ordinary custom and metadata entries retain their historical collection
/// semantics. Native metadata ancestry is O(history); custom tree/storage trait
/// fallbacks may hydrate full entries unless they override `branch_ancestry`.
pub async fn collect_entries_for_branch_summary<S: SessionTree + ?Sized>(
    session: &S,
    old_leaf_id: Option<&str>,
    target_id: &str,
) -> SessionResult<CollectEntriesResult> {
    let old_leaf_id = match old_leaf_id {
        Some(id) => id,
        None => {
            return Ok(CollectEntriesResult {
                entries: Vec::new(),
                common_ancestor_id: None,
            })
        }
    };

    // Identity must include archival nodes: filtering these paths would move
    // the reported LCA backward when the fork is rooted at an archive entry.
    let old_path: BTreeSet<String> = session
        .branch_ancestry(&BranchBounds {
            start: Some(old_leaf_id.to_string()),
            ..Default::default()
        })
        .await?
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    let target_path = session
        .branch_ancestry(&BranchBounds {
            start: Some(target_id.to_string()),
            ..Default::default()
        })
        .await?;
    let common_ancestor_id = target_path
        .into_iter()
        .find(|entry| old_path.contains(&entry.id))
        .map(|entry| entry.id);

    // Read only the abandoned tail's semantic payloads. Use newest-first for
    // inclusive stop bounds (oldest-first queries have historical truncation
    // semantics), remove the ancestor itself, then restore chronological order.
    // Ordinary custom/metadata entries remain part of the public collection.
    let mut entries = session
        .find_entries_on_branch(
            &EntryQuery {
                exclude_tool_program_bodies: true,
                order: Some(EntryOrder::NewestFirst),
                ..Default::default()
            },
            &BranchBounds {
                start: Some(old_leaf_id.to_string()),
                stop_at_id: common_ancestor_id.clone(),
                ..Default::default()
            },
        )
        .await?;
    entries.retain(|entry| Some(entry.id()) != common_ancestor_id.as_deref());
    entries.reverse();
    Ok(CollectEntriesResult {
        entries,
        common_ancestor_id,
    })
}

/// Prepare branch entries for summarization within an optional token budget.
/// Mirrors `prepareBranchEntries`: seed file ops from prior branch_summary
/// details, then walk back accumulating messages until the budget is exceeded
/// (compaction/branch_summary entries get a 0.9×budget grace before dropping).
pub fn prepare_branch_entries(entries: &[Entry], token_budget: i64) -> BranchPreparation {
    let mut messages: Vec<AgentMessage> = Vec::new();
    let mut file_ops = create_file_ops();
    let mut total_tokens = 0i64;

    // Seed file ops from prior branch_summary details.
    for entry in entries {
        if let Entry::BranchSummary(b) = entry {
            if let Some(details_value) = &b.details {
                if let Some(arr) = details_value.get("readFiles").and_then(|v| v.as_array()) {
                    for f in arr {
                        if let Some(s) = f.as_str() {
                            file_ops.read.insert(s.to_string());
                        }
                    }
                }
                if let Some(arr) = details_value
                    .get("modifiedFiles")
                    .and_then(|v| v.as_array())
                {
                    for f in arr {
                        if let Some(s) = f.as_str() {
                            file_ops.edited.insert(s.to_string());
                        }
                    }
                }
            }
        }
    }

    // Walk back, unshifting into chronological order.
    if token_budget > 0 {
        for entry in entries.iter().rev() {
            let message = match get_message_from_entry(entry) {
                Some(m) => m,
                None => continue,
            };
            extract_file_ops_from_message(&message, &mut file_ops);
            let tokens = estimate_tokens(&message);
            if total_tokens + tokens > token_budget {
                if matches!(entry, Entry::Compaction(_) | Entry::BranchSummary(_))
                    && total_tokens < (token_budget as f64 * 0.9) as i64
                {
                    messages.insert(0, message);
                    total_tokens += tokens;
                }
                break;
            }
            messages.insert(0, message);
            total_tokens += tokens;
        }
    } else {
        // No budget → include everything in chronological order.
        for entry in entries {
            if let Some(message) = get_message_from_entry(entry) {
                extract_file_ops_from_message(&message, &mut file_ops);
                total_tokens += estimate_tokens(&message);
                messages.push(message);
            }
        }
    }

    BranchPreparation {
        messages,
        file_ops,
        total_tokens,
    }
}

// ---------------------------------------------------------------------------
// Prompts — verbatim from branch-summarization.ts
// ---------------------------------------------------------------------------

const BRANCH_SUMMARY_PREAMBLE: &str =
    "The user explored a different conversation branch before returning here.\nSummary of that exploration:\n\n";

const BRANCH_SUMMARY_PROMPT: &str = "Create a structured summary of this conversation branch for context when returning later.\n\nUse this EXACT format:\n\n## Goal\n[What was the user trying to accomplish in this branch?]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Work that was started but not finished]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [What should happen next to continue this work]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// `generateBranchSummary`. Mirrors TS: budget = `context_window − reserve`
/// (context_window 0 → 128000); empty messages → `"No content to summarize"`;
/// single user message with the conversation+instructions prompt; `maxTokens:
/// 2048`; prepend `BRANCH_SUMMARY_PREAMBLE`; append file-op tags; map
/// `aborted`/`error` to `BranchSummaryError`.
pub async fn generate_branch_summary(
    entries: &[Entry],
    options: &GenerateBranchSummaryOptions,
) -> Result<BranchSummaryResult, BranchSummaryError> {
    let context_window = if options.llm.model.context_window > 0 {
        options.llm.model.context_window as i64
    } else {
        128_000
    };
    let token_budget = context_window - options.reserve_tokens;

    let prep = prepare_branch_entries(entries, token_budget);

    if prep.messages.is_empty() {
        return Ok(BranchSummaryResult {
            summary: "No content to summarize".to_string(),
            usage: None,
            read_files: Vec::new(),
            modified_files: Vec::new(),
        });
    }

    let llm_messages = convert_to_llm(prep.messages.clone());
    let conversation_text = serialize_conversation(&llm_messages);

    let instructions: String = match (
        &options.llm.custom_instructions,
        options.replace_instructions,
    ) {
        (Some(ci), true) => ci.clone(),
        (Some(ci), false) => format!("{BRANCH_SUMMARY_PROMPT}\n\nAdditional focus: {ci}"),
        (None, _) => BRANCH_SUMMARY_PROMPT.to_string(),
    };
    let prompt_text =
        format!("<conversation>\n{conversation_text}\n</conversation>\n\n{instructions}");

    let summarization_messages = vec![Message::User(UserMessage::new(
        UserContent::Blocks(vec![Content::text(prompt_text)]),
        now_ms(),
    ))];
    let context = Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: summarization_messages,
        tools: Vec::new(),
    };

    let mut req_opts = rpi_ai::SimpleStreamOptions::default();
    req_opts.api_key = options.llm.api_key.clone();
    req_opts.signal = options.llm.signal.clone();
    req_opts.max_tokens = Some(2048);
    if options.llm.model.reasoning {
        if let Some(tl) = options.llm.thinking_level {
            if tl != ThinkingLevel::Off {
                req_opts.reasoning = Some(tl);
            }
        }
    }

    let response = complete_simple_with_retries(
        options.llm.provider.clone(),
        options.llm.model.clone(),
        context,
        req_opts,
        options.llm.retry.clone(),
    )
    .await;

    if response.stop_reason == StopReason::Aborted {
        return Err(BranchSummaryError::aborted(
            response
                .error_message
                .as_deref()
                .unwrap_or("Branch summary aborted"),
        ));
    }
    if response.stop_reason == StopReason::Error {
        return Err(BranchSummaryError::summarization_failed(format!(
            "Branch summary failed: {}",
            response.error_message.as_deref().unwrap_or("Unknown error")
        )));
    }

    let mut summary = Content::text_only(&response.content, "\n");
    summary = format!("{BRANCH_SUMMARY_PREAMBLE}{summary}");
    let (read_files, modified_files) = compute_file_lists(&prep.file_ops);
    summary += &format_file_operations(&read_files, &modified_files);

    Ok(BranchSummaryResult {
        summary: if summary.is_empty() {
            "No summary generated".to_string()
        } else {
            summary
        },
        usage: Some(response.usage.clone()),
        read_files,
        modified_files,
    })
}

// Re-export the harness-level `CompactionLlmOptions` alias for callers building
// branch-summary options, and a tiny helper to construct the default reserve.
impl GenerateBranchSummaryOptions {
    /// Build options with a given LLM config and the default reserve.
    pub fn new(llm: CompactionLlmOptions) -> Self {
        Self {
            llm,
            replace_instructions: false,
            reserve_tokens: Self::DEFAULT_RESERVE_TOKENS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::types::EntryBase;

    fn user_msg(text: &str, seq: u64) -> Entry {
        Entry::Message(crate::session::types::MessageEntry {
            base: EntryBase {
                entry_type: "message".into(),
                id: format!("e{seq}"),
                seq,
                parent_id: None,
                timestamp: seq as i64,
            },
            message: AgentMessage::User(UserMessage::new(
                UserContent::Text(text.into()),
                seq as i64,
            )),
            terminate: None,
        })
    }

    #[test]
    fn prepare_branch_entries_no_budget_includes_all_non_toolresult() {
        let entries = vec![user_msg("a", 1), user_msg("b", 2)];
        let p = prepare_branch_entries(&entries, 0);
        assert_eq!(p.messages.len(), 2);
    }

    #[test]
    fn prepare_branch_entries_budget_drops_overflow_keeps_trailing_fit() {
        // Walk is back-to-front. Trailing "XYZ" (ceil(3/4)=1) fits in budget 3
        // and is unshifted; leading 26-char msg (ceil(26/4)=7) overflows
        // (1+7=8 > 3), is a plain user message (not compaction/branch_summary),
        // so it is dropped and the loop breaks. Result: only the trailing msg.
        let entries = vec![
            user_msg("abcdefghijklmnopqrstuvwxyz", 1),
            user_msg("XYZ", 2),
        ];
        let p = prepare_branch_entries(&entries, 3);
        assert_eq!(p.messages.len(), 1);
        assert!(matches!(
            &p.messages[0],
            AgentMessage::User(u) if matches!(u.content, UserContent::Text(ref t) if t == "XYZ")
        ));
    }

    #[test]
    fn prepare_branch_entries_seeds_from_prior_branch_summary_details() {
        let details = serde_json::json!({"readFiles": ["/r"], "modifiedFiles": ["/m"]});
        let b = Entry::BranchSummary(crate::session::types::BranchSummaryEntry {
            base: EntryBase {
                entry_type: "branch_summary".into(),
                id: "b1".into(),
                seq: 1,
                parent_id: None,
                timestamp: 1,
            },
            from_id: "leaf".into(),
            summary: "s".into(),
            details: Some(details),
            usage: None,
        });
        let p = prepare_branch_entries(&[b], 0);
        assert!(p.file_ops.read.contains("/r"));
        assert!(p.file_ops.edited.contains("/m"));
    }
}
