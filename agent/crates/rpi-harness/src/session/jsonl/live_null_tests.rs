//! Live-only null presence must survive cold hydration, not a legacy reopen.
use super::*;
use crate::session::{
    reducer::{entry_provisioned_json, matches_provisioned_entry},
    state::SessionState,
    types::provisioned_into_entry,
};
use rpi_agent::message::AgentMessage;
use rpi_ai::types::ToolResultRole;
use rpi_ai::{Api, AssistantMessage, DeferredHandle, ToolResultMessage};
use serde_json::Value;

fn messages(null: bool) -> Vec<AgentMessage> {
    let mut assistant = AssistantMessage::empty(Api::Faux, "faux", "model", 1);
    assistant.deferred = Some(DeferredHandle {
        provider: "faux".into(),
        model_id: "model".into(),
        api: "faux".into(),
        id: "deferred".into(),
        expires_at: None,
        poll_after_ms: None,
        data: null.then_some(Value::Null),
    });
    vec![
        ToolResultMessage {
            role: ToolResultRole,
            tool_call_id: "call".into(),
            tool_name: "tool".into(),
            content: Vec::new(),
            details: null.then_some(Value::Null),
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: 1,
        }
        .into(),
        assistant.into(),
    ]
}
fn provisioned() -> Vec<ProvisionedEntry> {
    let mut entries = Vec::new();
    for null in [true, false] {
        for (index, message) in messages(null).into_iter().enumerate() {
            entries.push(ProvisionedEntry {
                id: format!("message-{null}-{index}"),
                kind: ProvisionedKind::Message {
                    message,
                    terminate: None,
                },
            });
        }
    }
    // Interleave corrected and untouched slots: indices, not just message
    // kinds, must survive hydration of the retained tail.
    let present = messages(true);
    let absent = messages(false);
    entries.push(ProvisionedEntry {
        id: "compaction".into(),
        kind: ProvisionedKind::Compaction {
            summary: "summary".into(),
            retained_tail: vec![
                absent[0].clone(),
                present[1].clone(),
                present[0].clone(),
                absent[1].clone(),
            ],
            tokens_before: 10,
            details: None,
            usage: None,
        },
    });
    entries
}

#[tokio::test]
async fn appended_live_nulls_match_return_get_find_context_and_intent_but_reopen_keeps_none() {
    let fs = MemoryFs::new(Mode::Stable);
    let storage = create(fs.clone()).await;
    let mut eager = SessionState::new();
    let mut returned = Vec::new();
    let mut ids = Vec::new();
    for provisioned in provisioned() {
        let entry = storage.append_entry(provisioned, "main").await.unwrap();
        eager
            .apply_mutation(SessionMutation::Entry {
                seq: entry.seq(),
                timestamp: entry.base().timestamp,
                lane: Some("main".into()),
                entry: entry.clone(),
            })
            .unwrap();
        ids.push(entry.id().to_owned());
        storage.state.lock().await.assert_cold_shared(&ids);
        let intent = entry_provisioned_json(&entry);
        for _ in 0..2 {
            let mut hydrated = storage.get_entry(entry.id()).await.unwrap().unwrap();
            assert_eq!(hydrated, entry);
            assert!(matches_provisioned_entry(&hydrated, &intent));
            // Mutating a returned body cannot change subsequent hydration.
            match &mut hydrated {
                Entry::Message(message) => message.message = messages(false).remove(0),
                Entry::Compaction(compaction) => compaction.retained_tail.clear(),
                _ => unreachable!(),
            }
        }
        assert_eq!(
            storage.get_entry(entry.id()).await.unwrap(),
            Some(entry.clone())
        );
        assert_eq!(
            storage
                .context_on_branch(&BranchBounds::default(), entry.id())
                .await
                .unwrap()
                .messages,
            eager
                .context_on_branch(&BranchBounds::default(), entry.id())
                .unwrap()
                .messages
        );
        returned.push(entry);
    }
    let query = EntryQuery {
        order: Some(EntryOrder::OldestFirst),
        ..Default::default()
    };
    let found = storage.find_entries(&query).await.unwrap();
    assert_eq!(found, returned);
    for (actual, original) in found.iter().zip(&returned) {
        assert!(matches_provisioned_entry(
            actual,
            &entry_provisioned_json(original)
        ));
    }
    // Reopening deliberately follows the old decoder: JSON null becomes None.
    let reopened = load(fs).await.unwrap();
    reopened.state.lock().await.assert_cold_shared(&ids);
    let expected: Vec<Entry> = returned
        .iter()
        .map(|entry| serde_json::from_value(entry.to_flat_json()).unwrap())
        .collect();
    assert_ne!(
        expected, returned,
        "fixture must exercise Some(Null) loss on reopen"
    );
    assert_eq!(reopened.find_entries(&query).await.unwrap(), expected);
    for entry in &expected {
        assert_eq!(
            reopened.get_entry(entry.id()).await.unwrap().as_ref(),
            Some(entry)
        );
        match entry {
            Entry::Message(message) => assert_no_null(&message.message),
            Entry::Compaction(compaction) => {
                for message in &compaction.retained_tail {
                    assert_no_null(message);
                }
            }
            _ => unreachable!(),
        }
    }
}
fn assert_no_null(message: &AgentMessage) {
    match message {
        AgentMessage::ToolResult(message) => assert_eq!(message.details, None),
        AgentMessage::Assistant(message) => {
            assert_eq!(message.deferred.as_ref().unwrap().data, None)
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn candidate_receipt_preserves_original_payload_corrections_even_when_rebound() {
    let fs = MemoryFs::new(Mode::Stable);
    create(fs.clone()).await;
    let reader = fs.reader("/history.jsonl");
    let mut candidate = ResidentState::default();
    let mut originals = Vec::new();
    let mut bytes = Vec::new();
    for (index, provisioned) in provisioned().into_iter().enumerate() {
        let seq = index as u64 + 1;
        let entry = provisioned_into_entry(
            provisioned,
            seq,
            originals.last().map(|entry: &Entry| entry.id().to_owned()),
            1,
        );
        let mutation = SessionMutation::Entry {
            seq,
            timestamp: 1,
            lane: Some("main".into()),
            entry: entry.clone(),
        };
        bytes.extend_from_slice(encode_mutation(&mutation).as_bytes());
        candidate.apply_mutation(mutation).unwrap();
        originals.push(entry);
    }
    let offset = reader.bytes.lock().unwrap().len() as u64;
    reader.bytes.lock().unwrap().extend_from_slice(&bytes);
    // First bind captures resident originals; the second exercises an already
    // cold candidate. Neither may infer corrections from parse_mutation.
    for _ in 0..2 {
        candidate.bind_append(
            AppendReceipt {
                reader: reader.clone(),
                offset,
                length: bytes.len(),
            },
            &bytes,
        );
        candidate.assert_cold_shared(
            &originals
                .iter()
                .map(|entry| entry.id().to_owned())
                .collect::<Vec<_>>(),
        );
        for entry in &originals {
            assert_eq!(
                candidate
                    .entry_ref(entry.id())
                    .unwrap()
                    .read()
                    .await
                    .unwrap(),
                *entry
            );
        }
    }
}
