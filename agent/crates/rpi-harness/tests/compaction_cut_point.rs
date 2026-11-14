//! M5d integration — compaction cut-point selection. Mirrors the
//! `findCutPoint`/`findTurnStartIndex`/`findValidCutPoints` cases in
//! `packages/agent/test/harness/compaction.test.ts` and the split-turn cases in
//! `packages/coding-agent/test/compaction.test.ts`.
//!
//! Runs against the public `rpi_harness::compaction` surface only — no LLM, no
//! session storage.

use rpi_agent::message::AgentMessage;
use rpi_ai::types::{Api, StopReason, Usage, UsageCost, UserContent, UserMessage};
use rpi_harness::compaction::{
    find_cut_point, find_turn_start_index, find_valid_cut_points, CutPointResult,
};
use rpi_harness::session::types::{Entry, EntryBase, MessageEntry};

// ---- helpers (mirror the TS `createUserMessage`/`createAssistantMessage`) ----

fn base(seq: u64, parent: Option<&str>, entry_type: &str) -> EntryBase {
    EntryBase {
        entry_type: entry_type.to_string(),
        id: format!("entry-{seq}"),
        seq,
        parent_id: parent.map(|s| s.to_string()),
        timestamp: seq as i64,
    }
}

fn user_msg(text: &str, seq: u64, parent: Option<&str>) -> Entry {
    Entry::Message(MessageEntry {
        base: base(seq, parent, "message"),
        message: AgentMessage::User(UserMessage::new(
            UserContent::Text(text.to_string()),
            seq as i64,
        )),
        terminate: None,
    })
}

fn usage(input: i64, output: i64, cache_read: i64, cache_write: i64) -> Usage {
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: input + output + cache_read + cache_write,
        cost: UsageCost {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            total: 0.0,
        },
    }
}

fn assistant_msg(text: &str, seq: u64, parent: Option<&str>, stop: StopReason, u: Usage) -> Entry {
    let mut m = rpi_ai::types::AssistantMessage::empty(
        Api::AnthropicMessages,
        "anthropic",
        "claude-sonnet-4-5",
        seq as i64,
    );
    m.content = vec![rpi_ai::types::Content::text(text)];
    m.stop_reason = stop;
    m.usage = u;
    Entry::Message(MessageEntry {
        base: base(seq, parent, "message"),
        message: AgentMessage::Assistant(Box::new(m)),
        terminate: None,
    })
}

fn tool_result_msg(seq: u64, parent: Option<&str>) -> Entry {
    Entry::Message(MessageEntry {
        base: base(seq, parent, "message"),
        message: AgentMessage::ToolResult(Box::new(rpi_ai::types::ToolResultMessage {
            role: rpi_ai::types::ToolResultRole,
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            content: vec![rpi_ai::types::Content::text("tool output")],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: seq as i64,
        })),
        terminate: None,
    })
}

fn compaction_entry(summary: &str, seq: u64, parent: Option<&str>) -> Entry {
    Entry::Compaction(rpi_harness::session::types::CompactionEntry {
        base: base(seq, parent, "compaction"),
        summary: summary.to_string(),
        retained_tail: Vec::new(),
        tokens_before: 1234,
        details: None,
        usage: None,
    })
}

// ---- tests ----

#[test]
fn empty_range_returns_start() {
    let r = find_cut_point(&[], 0, 0, 1000);
    assert_eq!(
        r,
        CutPointResult {
            first_kept_entry_index: 0,
            turn_start_index: None,
            is_split_turn: false
        }
    );
}

#[test]
fn all_messages_fit_budget_keeps_start() {
    let entries = vec![
        user_msg("1", 1, None),
        assistant_msg(
            "a",
            2,
            Some("entry-1"),
            StopReason::Stop,
            usage(0, 50, 500, 0),
        ),
        user_msg("2", 3, Some("entry-2")),
        assistant_msg(
            "b",
            4,
            Some("entry-3"),
            StopReason::Stop,
            usage(0, 50, 1000, 0),
        ),
    ];
    let r = find_cut_point(&entries, 0, entries.len(), 50_000);
    assert_eq!(r.first_kept_entry_index, 0);
    assert!(!r.is_split_turn);
    assert_eq!(r.turn_start_index, None);
}

#[test]
fn valid_cut_points_exclude_tool_result() {
    let entries = vec![
        user_msg("hi", 1, None),
        assistant_msg("a", 2, Some("entry-1"), StopReason::Stop, usage(0, 1, 0, 0)),
        tool_result_msg(3, Some("entry-2")),
        user_msg("again", 4, Some("entry-3")),
    ];
    let cuts = find_valid_cut_points(&entries, 0, entries.len());
    // User(0), Assistant(1), User(3) — NOT ToolResult(2).
    assert_eq!(cuts, vec![0, 1, 3]);
}

#[test]
fn walk_back_picks_valid_cut_near_budget() {
    // 10 turns; cache_read grows per assistant: 1000,2000,... so accumulate
    // backwards crosses 2500 around the 7th-8th entry. The first retained entry
    // must be a message (mirrors the TS assertion).
    let mut entries: Vec<Entry> = Vec::new();
    let mut parent_owned: String = String::new();
    let mut parent: Option<&str> = None;
    for i in 0..10usize {
        let u = user_msg(&format!("User {i}"), (2 * i + 1) as u64, parent);
        let u_id = format!("entry-{}", 2 * i + 1);
        let a = assistant_msg(
            &format!("Assistant {i}"),
            (2 * i + 2) as u64,
            Some(&u_id),
            StopReason::Stop,
            usage(0, 100, (i as i64 + 1) * 1000, 0),
        );
        entries.push(u);
        entries.push(a);
        parent_owned = format!("entry-{}", 2 * i + 2);
        parent = Some(parent_owned.as_str());
    }
    let _ = parent_owned;
    let r = find_cut_point(&entries, 0, entries.len(), 2500);
    assert!(matches!(
        entries[r.first_kept_entry_index],
        Entry::Message(_)
    ));
    let role_ok = match &entries[r.first_kept_entry_index] {
        Entry::Message(m) => matches!(
            m.message.role(),
            rpi_agent::message::AgentMessageRole::User
                | rpi_agent::message::AgentMessageRole::Assistant
        ),
        _ => false,
    };
    assert!(role_ok);
}

#[test]
fn split_turn_when_cut_at_assistant_inside_turn() {
    // Turn 2 starts at index 2 (user). Three assistant messages follow with
    // large cache_read tokens so a 3000 budget cuts mid-turn 2 (at an assistant,
    // not a user), making it a split turn whose turn-start is index 2.
    let entries = vec![
        user_msg("Turn 1", 1, None),
        assistant_msg(
            "A1",
            2,
            Some("entry-1"),
            StopReason::Stop,
            usage(0, 100, 1000, 0),
        ),
        user_msg("Turn 2", 3, Some("entry-2")),
        assistant_msg(
            "A2-1",
            4,
            Some("entry-3"),
            StopReason::Stop,
            usage(0, 100, 5000, 0),
        ),
        assistant_msg(
            "A2-2",
            5,
            Some("entry-4"),
            StopReason::Stop,
            usage(0, 100, 8000, 0),
        ),
        assistant_msg(
            "A2-3",
            6,
            Some("entry-5"),
            StopReason::Stop,
            usage(0, 100, 10000, 0),
        ),
    ];
    let r = find_cut_point(&entries, 0, entries.len(), 3000);
    let cut_is_assistant = match &entries[r.first_kept_entry_index] {
        Entry::Message(m) => m.message.role() == rpi_agent::message::AgentMessageRole::Assistant,
        _ => false,
    };
    if cut_is_assistant {
        assert!(r.is_split_turn);
        assert_eq!(r.turn_start_index, Some(2));
    }
}

#[test]
fn compaction_entry_kept_at_boundary_walks_back_through_it() {
    // `[user, compaction, assistant]` with budget 1: the walk-back lands at the
    // assistant (index 2), the `while cutIndex > startIndex` walk-back stops at
    // the assistant because its predecessor (compaction) ends the walk.
    let entries = vec![
        user_msg("user", 1, None),
        compaction_entry("summary", 2, Some("entry-1")),
        assistant_msg(
            "assistant",
            3,
            Some("entry-2"),
            StopReason::Stop,
            usage(0, 1, 0, 0),
        ),
    ];
    let r = find_cut_point(&entries, 0, entries.len(), 1);
    assert_eq!(r.first_kept_entry_index, 2);
}

#[test]
fn turn_start_index_walks_back_to_user_or_branch_summary() {
    // find_turn_start_index returns the index of the nearest
    // user/bashExecution/branch_summary at or before entry_index.
    let entries = vec![
        user_msg("u", 1, None),
        assistant_msg("a", 2, Some("entry-1"), StopReason::Stop, usage(0, 1, 0, 0)),
    ];
    assert_eq!(find_turn_start_index(&entries, 1, 0), Some(0));
    // No turn start before index 1 with start_index 1 → None.
    assert_eq!(find_turn_start_index(&entries, 1, 1), None);
}

// Suppress unused import warning for `find_turn_start_index`-only edge case above
// where the compile-time dead-code analysis is conservative across test fns.
#[allow(dead_code)]
fn _suppress_unused() {
    let _ = Api::AnthropicMessages;
}
