//! The same reducer must work with payloads that cannot masquerade as Entries.
use super::*;
use crate::session::types::{
    AssistantFrameOp, AssistantFrameRecord, CustomEntry, EntryBase, MessageEntry,
    MessageSummaryQuery, MessageWindowQuery, RecordBase,
};

#[derive(Clone, Debug, PartialEq)]
struct ColdRef(u64); // Deliberately has neither Entry methods nor serde support.

fn user(seq: u64, id: &str, parent: Option<&str>) -> SessionMutation {
    SessionMutation::Entry {
        seq,
        timestamp: 42,
        lane: Some("main".into()),
        entry: Entry::Message(MessageEntry {
            base: EntryBase {
                entry_type: "message".into(),
                id: id.into(),
                seq,
                parent_id: parent.map(str::to_owned),
                timestamp: 42,
            },
            message: rpi_ai::UserMessage::new("private payload 🐈".repeat(100), 43).into(),
            terminate: None,
        }),
    }
}

fn mutations() -> Vec<SessionMutation> {
    vec![
        user(1, "a", None),
        user(2, "b", Some("a")),
        SessionMutation::Record {
            record: LaneRecord::AssistantFrame(AssistantFrameRecord {
                base: RecordBase {
                    id: "frame".into(),
                    seq: 3,
                    lane: "main".into(),
                    timestamp: 42,
                },
                run_id: "run".into(),
                stream_index: 0,
                op: AssistantFrameOp::ClearRun,
                frame: None,
            }),
        },
        SessionMutation::FactName {
            seq: 4,
            name: Some("shared".into()),
        },
        SessionMutation::FactLabel {
            seq: 5,
            target_id: "a".into(),
            label: Some("label".into()),
        },
        SessionMutation::Lane {
            seq: 6,
            lane: "sibling".into(),
            leaf_id: Some("a".into()),
        },
        user(7, "c", Some("b")),
    ]
}

#[test]
fn cold_and_eager_replay_have_identical_metadata_and_selections() {
    let mut cold = IndexedState::<ColdRef>::new();
    let mut eager = SessionState::new();
    for mutation in mutations() {
        let eager_outcome = eager.apply_mutation(mutation.clone()).unwrap();
        let cold_outcome = cold
            .apply_mutation(mutation, |entry| ColdRef(entry.seq()))
            .unwrap();
        assert_eq!(
            std::mem::discriminant(&eager_outcome),
            std::mem::discriminant(&cold_outcome)
        );
        assert_eq!(cold.metadata(), eager.index.metadata());
        assert_eq!(cold.get_lanes(), eager.get_lanes());
        assert_eq!(cold.get_stats(), eager.get_stats());
        assert_eq!(cold.sequence(), eager.sequence());
        assert_eq!(cold.get_name(), eager.get_name());
        assert_eq!(cold.get_label("a"), eager.get_label("a"));
        assert_eq!(cold.records(), eager.index.records());
    }
    let query = MessageSummaryQuery::default();
    let selected = cold.select_message_summaries("main", &query).unwrap();
    let page = eager.message_summaries("main", &query).unwrap();
    let ids: Vec<_> = selected
        .indices
        .iter()
        .map(|&i| cold.entry_meta(i).unwrap().id())
        .collect();
    assert_eq!(
        ids,
        page.messages
            .iter()
            .map(|message| message.entry_id.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(selected.has_more, page.has_more);
    let query = MessageWindowQuery {
        anchor_id: Some("b".into()),
        limit: 2,
        ..Default::default()
    };
    let selected = cold.select_message_window("main", &query).unwrap();
    let page = eager.message_window("main", &query).unwrap();
    assert_eq!(
        selected
            .indices
            .iter()
            .map(|&i| cold.entry_meta(i).unwrap().id())
            .collect::<Vec<_>>(),
        page.entries.iter().map(Entry::id).collect::<Vec<_>>()
    );
    assert_eq!(
        (selected.has_more_before, selected.has_more_after),
        (page.has_more_before, page.has_more_after)
    );
    assert_eq!(
        cold.walk_to_root(Some("c"), &BranchBounds::default())
            .unwrap(),
        vec![2, 1, 0]
    );
    assert_eq!(cold.payload(0), Some(&ColdRef(1)));
    assert_eq!(cold.metadata()[0].message_timestamp, Some(43));
    assert_eq!(
        cold.log_items(&LogOptions::default()).unwrap().len(),
        eager.get_log(&LogOptions::default()).unwrap().len()
    );
}

#[test]
fn rejected_mutations_never_construct_payload_or_change_metadata() {
    let mut cold = IndexedState::<ColdRef>::new();
    cold.apply_mutation(user(1, "a", None), |_| ColdRef(1))
        .unwrap();
    let mut missing_lane = user(2, "b", Some("a"));
    if let SessionMutation::Entry { lane, .. } = &mut missing_lane {
        *lane = Some("missing".into());
    }
    let mut missing_parent = user(2, "b", Some("missing"));
    if let SessionMutation::Entry { lane, .. } = &mut missing_parent {
        *lane = None;
    }
    for mutation in [
        user(9, "b", Some("a")),
        user(2, "a", Some("a")),
        user(2, "b", None),
        missing_lane,
        missing_parent,
        SessionMutation::Lane {
            seq: 2,
            lane: "other".into(),
            leaf_id: Some("missing".into()),
        },
        SessionMutation::FactLabel {
            seq: 2,
            target_id: "missing".into(),
            label: None,
        },
    ] {
        let before = cold.metadata().to_vec();
        assert!(cold
            .apply_mutation(mutation, |_| panic!(
                "invalid mutation invoked payload factory"
            ))
            .is_err());
        assert_eq!(cold.metadata(), before);
        assert_eq!(cold.sequence(), 1);
        assert_eq!(cold.require_lane("main").unwrap().as_deref(), Some("a"));
    }
    let before = cold.metadata().to_vec();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = cold.apply_mutation(user(2, "b", Some("a")), |_| {
            panic!("payload construction failed")
        });
    }));
    assert!(panic.is_err());
    assert_eq!(cold.metadata(), before);
    assert_eq!(cold.sequence(), 1);
}

#[test]
fn metadata_is_not_a_fabricated_entry_and_payload_filters_are_deferred() {
    let mut cold = IndexedState::<ColdRef>::new();
    let entry = Entry::Custom(CustomEntry {
        base: EntryBase {
            entry_type: "custom".into(),
            id: "custom".into(),
            seq: 1,
            parent_id: None,
            timestamp: 42,
        },
        custom_type: "opaque".into(),
        data: Some(serde_json::json!({"secret": "not retained in metadata"})),
    });
    cold.apply_mutation(
        SessionMutation::Entry {
            seq: 1,
            timestamp: 42,
            lane: Some("main".into()),
            entry: entry.clone(),
        },
        |_| ColdRef(100),
    )
    .unwrap();
    let query = EntryQuery {
        tool_call_id: Some("unknown".into()),
        outbox_key: Some("unknown".into()),
        limit: Some(1),
        ..Default::default()
    };
    assert_eq!(cold.select_entry_indices(&query, None).unwrap(), vec![0]);
    assert!(!matches_entry_query(&entry, &query));
    assert_eq!(
        cold.entry_meta(0).unwrap().custom_type.as_deref(),
        Some("opaque")
    );
    // A cold payload has no Entry API; metadata exposes no as_message/base_mut
    // or conversion to Entry. Even diagnostics contain no custom JSON payload.
    assert!(!format!("{:?}", cold.metadata()).contains("not retained"));
    *cold.payload_mut(0).unwrap() = ColdRef(101);
    assert_eq!(cold.entry_meta(0).unwrap().id(), "custom");
}
