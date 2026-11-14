use super::*;
use crate::session::types::{BranchBounds, EntryBase, EntryQuery, MessageEntry, SessionMutation};
use rpi_ai::types::{Api, AssistantMessage, Content, UserContent, UserMessage};
fn append(state: &mut SessionState, id: &str, user: bool) {
    let seq = state.next_sequence();
    let message = if user {
        UserMessage::new(format!("Question {id} 🐈"), seq as i64).into()
    } else {
        let mut msg = AssistantMessage::empty(Api::AnthropicMessages, "fixture", "fixture", 0);
        msg.content
            .push(Content::text("tool-heavy output ".repeat(100)));
        AgentMessage::Assistant(Box::new(msg))
    };
    let entry = Entry::Message(MessageEntry {
        base: EntryBase {
            entry_type: "message".into(),
            id: id.into(),
            seq,
            parent_id: state.index.lanes["main"].clone(),
            timestamp: seq as i64,
        },
        message,
        terminate: None,
    });
    state
        .apply_mutation(SessionMutation::Entry {
            seq,
            timestamp: seq as i64,
            lane: Some("main".into()),
            entry,
        })
        .unwrap();
}
fn state() -> SessionState {
    let mut state = SessionState::new();
    for i in 0..25 {
        append(&mut state, &format!("u{i}"), true);
        for j in 0..20 {
            append(&mut state, &format!("t{i}-{j}"), false);
        }
    }
    state
}
fn move_to(state: &mut SessionState, id: &str) {
    state
        .apply_mutation(SessionMutation::Lane {
            seq: state.next_sequence(),
            lane: "main".into(),
            leaf_id: Some(id.into()),
        })
        .unwrap();
}
#[test]
fn pagination_uses_only_users_and_is_exclusive_oldest_first() {
    let state = state();
    let mut query = MessageSummaryQuery::default();
    let first = state.message_summaries("main", &query).unwrap();
    assert_eq!(first.messages.len(), 10);
    assert_eq!(first.messages.first().unwrap().entry_id, "u15");
    assert_eq!(first.messages.last().unwrap().entry_id, "u24");
    assert_eq!(first.next_before_id.as_deref(), Some("u15"));
    query.leaf_id = first.leaf_id;
    query.before_id = first.next_before_id;
    let second = state.message_summaries("main", &query).unwrap();
    assert_eq!(second.first_id.as_deref(), Some("u5"));
    assert_eq!(second.last_id.as_deref(), Some("u14"));
    query.before_id = second.next_before_id;
    let third = state.message_summaries("main", &query).unwrap();
    assert_eq!(third.messages.len(), 5);
    assert!(!third.has_more);
    assert!(third.next_before_id.is_none());
    query.before_id = third.first_id;
    assert!(state
        .message_summaries("main", &query)
        .unwrap()
        .messages
        .is_empty());
}
#[test]
fn empty_single_and_invalid_queries_are_bounded() {
    let mut state = SessionState::new();
    assert!(state
        .message_summaries("main", &Default::default())
        .unwrap()
        .messages
        .is_empty());
    append(&mut state, "one", true);
    assert!(
        !state
            .message_summaries("main", &Default::default())
            .unwrap()
            .has_more
    );
    for limit in [0, 101, usize::MAX] {
        assert!(state
            .message_summaries(
                "main",
                &MessageSummaryQuery {
                    limit,
                    ..Default::default()
                }
            )
            .is_err());
    }
    assert!(state
        .message_summaries(
            "main",
            &MessageSummaryQuery {
                role: "assistant".into(),
                ..Default::default()
            }
        )
        .is_err());
    for id in ["", "unknown", "\n"] {
        assert!(state
            .message_summaries(
                "main",
                &MessageSummaryQuery {
                    before_id: Some(id.into()),
                    ..Default::default()
                }
            )
            .is_err());
    }
    assert!(state
        .message_summaries("other", &Default::default())
        .is_err());
}
#[test]
fn pinned_leaf_allows_appends_but_rejects_rewind_and_sibling_cursors() {
    let mut state = state();
    let first = state
        .message_summaries("main", &Default::default())
        .unwrap();
    append(&mut state, "new", true);
    let query = MessageSummaryQuery {
        leaf_id: first.leaf_id.clone(),
        ..Default::default()
    };
    assert_eq!(
        state
            .message_summaries("main", &query)
            .unwrap()
            .last_id
            .as_deref(),
        Some("u24")
    );
    move_to(&mut state, "u5");
    append(&mut state, "sibling", true);
    assert!(state
        .message_summaries("main", &query)
        .unwrap_err()
        .message
        .contains("branch_changed"));
    assert!(state
        .message_summaries(
            "main",
            &MessageSummaryQuery {
                before_id: Some("u24".into()),
                ..Default::default()
            }
        )
        .is_err());
    assert_eq!(
        state
            .message_summaries("main", &Default::default())
            .unwrap()
            .last_id
            .as_deref(),
        Some("sibling")
    );
}
#[test]
fn snapshot_rebuilds_index_without_persisting_it() {
    let state = state();
    let json = serde_json::to_value(&state).unwrap();
    assert!(json.get("nearest_user").is_none());
    let restored: SessionState = serde_json::from_value(json).unwrap();
    assert_eq!(
        restored
            .message_summaries("main", &Default::default())
            .unwrap()
            .first_id
            .as_deref(),
        Some("u15")
    );
}
#[test]
fn preview_is_unicode_bounded_and_does_not_contain_image_payloads() {
    let mut state = SessionState::new();
    append(&mut state, "image", true);
    if let Entry::Message(entry) = &mut state.index.entries[0] {
        entry.message = UserMessage::new(UserContent::Blocks(vec![Content::text(" 猫🐈 ".repeat(5000)), serde_json::from_value(serde_json::json!({"type":"image","data":"secret-base64","mimeType":"image/png"})).unwrap()]), 10).into();
    }
    let page = state
        .message_summaries("main", &Default::default())
        .unwrap();
    assert!(page.messages[0].has_images);
    assert!(page.messages[0].preview.chars().count() <= 140);
    assert!(!serde_json::to_string(&page)
        .unwrap()
        .contains("secret-base64"));
}
#[test]
fn context_anchor_and_both_cursor_directions_have_no_overlap() {
    let state = state();
    let middle = state
        .message_window(
            "main",
            &MessageWindowQuery {
                anchor_id: Some("u12".into()),
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(middle.entries.len(), 10);
    assert!(middle.entries.iter().any(|entry| entry.id() == "u12"));
    assert!(middle.has_more_before && middle.has_more_after);
    let before = state
        .message_window(
            "main",
            &MessageWindowQuery {
                before_id: middle.next_before_id.clone(),
                leaf_id: middle.leaf_id.clone(),
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
    let after = state
        .message_window(
            "main",
            &MessageWindowQuery {
                after_id: middle.next_after_id.clone(),
                leaf_id: middle.leaf_id.clone(),
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(before.entries.last().unwrap().seq() < middle.entries.first().unwrap().seq());
    assert!(after.entries.first().unwrap().seq() > middle.entries.last().unwrap().seq());
    assert!(state
        .message_window("main", &MessageWindowQuery::default())
        .is_err());
    assert!(state
        .message_window(
            "main",
            &MessageWindowQuery {
                anchor_id: Some("u0".into()),
                before_id: Some("u1".into()),
                ..Default::default()
            }
        )
        .is_err());
}
#[test]
#[ignore = "release-mode synthetic benchmark; no provider or real session"]
fn benchmark_borrowed_user_index_against_full_projection() {
    let mut state = SessionState::new();
    for i in 0..100 {
        append(&mut state, &format!("u{i}"), true);
        for j in 0..100 {
            append(&mut state, &format!("t{i}-{j}"), false);
        }
    }
    let start = std::time::Instant::now();
    let all = state
        .find_entries_on_branch(
            &EntryQuery::default(),
            &BranchBounds::default(),
            state.index.lanes["main"].as_deref().unwrap(),
        )
        .unwrap();
    let full = serde_json::to_vec(&all).unwrap();
    let legacy = start.elapsed();
    let start = std::time::Instant::now();
    let summary = state
        .message_summaries("main", &Default::default())
        .unwrap();
    let bytes = serde_json::to_vec(&summary).unwrap();
    println!(
        "synthetic 10,100 entries: full projection {legacy:?} {} bytes; 10-user summary {:?} {} bytes",
        full.len(),
        start.elapsed(),
        bytes.len()
    );
    assert_eq!(summary.messages.len(), 10);
}

#[test]
fn context_windows_match_reference_ranges_at_every_boundary() {
    let mut state = SessionState::new();
    for i in 0..31 {
        append(&mut state, &format!("m{i}"), i % 3 == 0);
    }
    for limit in 1..=40 {
        for index in 0usize..31 {
            let id = format!("m{index}");
            for mode in 0..3 {
                let query = MessageWindowQuery {
                    limit,
                    anchor_id: (mode == 0).then(|| id.clone()),
                    before_id: (mode == 1).then(|| id.clone()),
                    after_id: (mode == 2).then(|| id.clone()),
                    ..Default::default()
                };
                let page = state.message_window("main", &query).unwrap();
                let (start, end) = match mode {
                    0 => {
                        let start = index
                            .saturating_sub(limit / 3)
                            .min(31usize.saturating_sub(limit));
                        (start, (start + limit).min(31))
                    }
                    1 => (index.saturating_sub(limit), index),
                    _ => (index + 1, (index + 1 + limit).min(31)),
                };
                assert_eq!(
                    page.entries
                        .iter()
                        .map(|entry| entry.id().to_owned())
                        .collect::<Vec<_>>(),
                    (start..end).map(|i| format!("m{i}")).collect::<Vec<_>>()
                );
                assert_eq!(page.has_more_before, start > 0 && start != end);
                assert_eq!(page.has_more_after, end < 31 && start != end);
            }
        }
    }
}
#[test]
fn queries_are_lane_scoped_and_reject_foreign_message_cursors() {
    let mut state = state();
    state
        .apply_mutation(SessionMutation::Lane {
            seq: state.next_sequence(),
            lane: "other".into(),
            leaf_id: state.index.lanes["main"].clone(),
        })
        .unwrap();
    move_to(&mut state, "u5");
    append(&mut state, "sibling", true);
    assert_eq!(
        state
            .message_summaries("other", &Default::default())
            .unwrap()
            .last_id
            .as_deref(),
        Some("u24")
    );
    assert!(state
        .message_window(
            "main",
            &MessageWindowQuery {
                anchor_id: Some("u24".into()),
                ..Default::default()
            }
        )
        .is_err());
    assert!(state
        .message_summaries(
            "other",
            &MessageSummaryQuery {
                before_id: Some("sibling".into()),
                ..Default::default()
            }
        )
        .is_err());
}
