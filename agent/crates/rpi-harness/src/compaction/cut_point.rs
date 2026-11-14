//! Mirrors `findValidCutPoints`/`findTurnStartIndex`/`findCutPoint` from
//! `packages/agent/src/harness/compaction/compaction.ts` (lines 312–422).
//!
//! A "valid cut point" is an entry index at which it is safe to *start
//! retaining* history: a turn-starting message (user, assistant, bashExecution,
//! custom, branchSummary, compactionSummary) — **NEVER a toolResult**, whose
//! meaning depends on a preceding assistant tool call — or a `branch_summary`
//! entry. `find_cut_point` then walks back from the end accumulating estimated
//! tokens until the recent-context budget is met, and picks the smallest valid
//! cut at or beyond that index; if the cut lands inside a turn (a non-user
//! message) it also returns the turn's start so compaction can summarize the
//! turn prefix separately (the split-turn two-LLM-call path, invariant §10).

use rpi_agent::message::AgentMessageRole;

use crate::compaction::tokens::estimate_tokens;
use crate::messages::{
    BASH_EXECUTION_ROLE, BRANCH_SUMMARY_ROLE, COMPACTION_SUMMARY_ROLE, CUSTOM_ROLE,
};
use crate::session::types::Entry;

/// `true` if `entry` is a valid compaction cut point. Mirrors the per-entry
/// check in `findValidCutPoints`: a message whose role is in the cut set (never
/// `toolResult`), or a `branch_summary` entry (regardless of content).
fn is_valid_cut_point(entry: &Entry) -> bool {
    match entry {
        Entry::Message(m) => match m.message.role() {
            AgentMessageRole::User | AgentMessageRole::Assistant => true,
            AgentMessageRole::Custom(role) => matches!(
                role.as_str(),
                BASH_EXECUTION_ROLE | CUSTOM_ROLE | BRANCH_SUMMARY_ROLE | COMPACTION_SUMMARY_ROLE
            ),
            AgentMessageRole::ToolResult => false,
        },
        Entry::BranchSummary(_) => true,
        // thinking_level_change / model_change / active_tools_change / compaction
        // / custom-entry are NOT valid cut points (the `break` arms in TS).
        _ => false,
    }
}

/// Collect the indices in `[start_index, end_index)` that are valid cut points.
/// Mirrors `findValidCutPoints`.
pub fn find_valid_cut_points(
    entries: &[Entry],
    start_index: usize,
    end_index: usize,
) -> Vec<usize> {
    let mut out = Vec::new();
    let end = end_index.min(entries.len());
    for i in start_index..end {
        if is_valid_cut_point(&entries[i]) {
            out.push(i);
        }
    }
    out
}

/// Find the user-visible message that starts the turn containing `entry_index`.
/// Mirrors `findTurnStartIndex`: walk back to the first `branch_summary`, or a
/// message whose role is `user`/`bashExecution`; return `-1` (here `None`) if
/// none found before `start_index`.
pub fn find_turn_start_index(
    entries: &[Entry],
    entry_index: usize,
    start_index: usize,
) -> Option<usize> {
    if entry_index < start_index {
        return None;
    }
    let mut i = entry_index;
    loop {
        match &entries[i] {
            Entry::BranchSummary(_) => return Some(i),
            Entry::Message(m) => match m.message.role() {
                AgentMessageRole::User => return Some(i),
                AgentMessageRole::Custom(role) if role == BASH_EXECUTION_ROLE => return Some(i),
                _ => {}
            },
            _ => {}
        }
        if i == start_index {
            break;
        }
        i -= 1;
    }
    None
}

/// Cut point selected for compaction. Mirrors `CutPointResult`. Fields use
/// `Option<usize>` where TS uses `-1` as "no turn start".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CutPointResult {
    /// Index of the first entry retained after compaction.
    pub first_kept_entry_index: usize,
    /// Index of the turn-start entry when the cut splits a turn.
    pub turn_start_index: Option<usize>,
    /// Whether the selected cut point splits an in-progress turn.
    pub is_split_turn: bool,
}

/// Find the compaction cut point that keeps approximately the requested
/// recent-token budget. Mirrors `findCutPoint`.
pub fn find_cut_point(
    entries: &[Entry],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: i64,
) -> CutPointResult {
    let cut_points = find_valid_cut_points(entries, start_index, end_index);

    if cut_points.is_empty() {
        return CutPointResult {
            first_kept_entry_index: start_index,
            turn_start_index: None,
            is_split_turn: false,
        };
    }

    let mut accumulated_tokens = 0i64;
    let mut cut_index = cut_points[0];

    // Walk back from endIndex-1 to startIndex, summing message tokens.
    if end_index > 0 {
        let start = start_index.min(entries.len());
        let mut i = end_index.min(entries.len());
        while i > start {
            i -= 1;
            let entry = &entries[i];
            let message = match entry.as_message() {
                Some(m) => m,
                None => continue,
            };
            let message_tokens = estimate_tokens(message);
            accumulated_tokens += message_tokens;
            if accumulated_tokens >= keep_recent_tokens {
                // Pick the smallest valid cut point >= i.
                for &c in &cut_points {
                    if c >= i {
                        cut_index = c;
                        break;
                    }
                }
                break;
            }
        }
    }

    // Walk cut_index back across non-message/non-compaction entries to land on
    // a meaningful boundary (mirrors the TS `while (cutIndex > startIndex)`).
    while cut_index > start_index {
        let prev = &entries[cut_index - 1];
        match prev {
            Entry::Compaction(_) | Entry::Message(_) => break,
            _ => cut_index -= 1,
        }
    }

    let cut_entry = &entries[cut_index];
    let is_user_message =
        matches!(cut_entry, Entry::Message(m) if m.message.role() == AgentMessageRole::User);
    let turn_start_index = if is_user_message {
        None
    } else {
        find_turn_start_index(entries, cut_index, start_index)
    };
    let is_split_turn = !is_user_message && turn_start_index.is_some();

    CutPointResult {
        first_kept_entry_index: cut_index,
        turn_start_index,
        is_split_turn,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::types::{EntryBase, MessageEntry};
    use rpi_agent::message::AgentMessage;
    use rpi_ai::types::{UserContent, UserMessage};

    fn base(seq: u64) -> EntryBase {
        EntryBase {
            entry_type: "message".into(),
            id: format!("e{seq}"),
            seq,
            parent_id: None,
            timestamp: seq as i64,
        }
    }

    fn user_msg(text: &str, seq: u64) -> Entry {
        Entry::Message(MessageEntry {
            base: base(seq),
            message: AgentMessage::User(UserMessage::new(
                UserContent::Text(text.into()),
                seq as i64,
            )),
            terminate: None,
        })
    }

    #[test]
    fn no_cut_points_returns_start() {
        // Empty slice → start_index == end_index == 0.
        let r = find_cut_point(&[], 0, 0, 100);
        assert_eq!(r.first_kept_entry_index, 0);
        assert!(!r.is_split_turn);
    }

    #[test]
    fn valid_cut_points_exclude_tool_result() {
        // user, assistant(toolCall implicit), toolResult, user.
        let mut assistant =
            rpi_ai::types::AssistantMessage::empty(rpi_ai::types::Api::Faux, "faux", "faux", 2);
        assistant.stop_reason = rpi_ai::types::StopReason::Stop;
        let entries = vec![
            user_msg("hi", 1),
            Entry::Message(MessageEntry {
                base: base(2),
                message: AgentMessage::Assistant(Box::new(assistant)),
                terminate: None,
            }),
            Entry::Message(MessageEntry {
                base: base(3),
                message: AgentMessage::ToolResult(Box::new(rpi_ai::types::ToolResultMessage {
                    role: rpi_ai::types::ToolResultRole,
                    tool_call_id: "c1".into(),
                    tool_name: "read".into(),
                    content: vec![rpi_ai::types::Content::text("out")],
                    details: None,
                    usage: None,
                    added_tool_names: Vec::new(),
                    is_error: false,
                    timestamp: 3,
                })),
                terminate: None,
            }),
            user_msg("again", 4),
        ];
        let cuts = find_valid_cut_points(&entries, 0, entries.len());
        // indices 0 (user), 1 (assistant), 3 (user) — NOT 2 (toolResult).
        assert_eq!(cuts, vec![0, 1, 3]);
    }
}
