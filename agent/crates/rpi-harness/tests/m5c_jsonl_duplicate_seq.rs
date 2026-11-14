//! Duplicate-seq recovery for `JsonlSessionStorage::load`.
//!
//! Exact repeated mutations may be removed during recovery, while a different
//! mutation carrying an already-used sequence is corruption: load must reject it
//! and leave the source bytes unchanged. Forward gaps remain accepted and a
//! stale entry parent is re-chained to the current lane leaf.

use std::sync::Arc;

use rpi_agent::message::AgentMessage;
use rpi_harness::session::jsonl::{
    encode_mutation, parse_mutation, JsonlSessionStorage, JsonlV4Header,
};
use rpi_harness::session::memory::{CounterIdGenerator, FakeClock};
use rpi_harness::session::types::{
    provisioned_into_entry, ProvisionedEntry, ProvisionedKind, SessionMutation, SessionStorage,
};
use rpi_tools::env::{FileContent, FileSystem};

fn user_msg(text: &str) -> AgentMessage {
    AgentMessage::User(rpi_ai::types::UserMessage::new(text, 1))
}

fn header() -> JsonlV4Header {
    JsonlV4Header::new(
        "sess-1".into(),
        1_700_000_000_000,
        "/cwd".into(),
        None,
        None,
        None,
    )
}

type Fixture = (Arc<dyn FileSystem>, Arc<FakeClock>, Arc<CounterIdGenerator>);

fn fixture() -> Fixture {
    let env = rpi_tools::InMemoryExecutionEnv::with_cwd("/".into());
    let fs: Arc<dyn FileSystem> = Arc::new(env);
    (
        fs,
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    )
}

fn message_provisioned(id: &str, text: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.to_string(),
        kind: ProvisionedKind::Message {
            message: user_msg(text),
            terminate: None,
        },
    }
}

/// A stamped `Entry` mutation on the `main` lane, chained to `parent`.
fn entry_mutation(seq: u64, id: &str, parent: Option<&str>) -> SessionMutation {
    let timestamp = 1_700_000_000_000i64 + seq as i64;
    SessionMutation::Entry {
        seq,
        timestamp,
        lane: Some("main".into()),
        entry: provisioned_into_entry(
            message_provisioned(id, id),
            seq,
            parent.map(str::to_string),
            timestamp,
        ),
    }
}

fn lane_mutation(seq: u64) -> SessionMutation {
    SessionMutation::Lane {
        seq,
        lane: "main".into(),
        leaf_id: None,
    }
}

fn line(mutation: &SessionMutation) -> String {
    encode_mutation(mutation)
}

#[tokio::test]
async fn duplicate_seq_is_dropped_gap_is_accepted_and_file_is_republished() {
    let (fs, clock, ids) = fixture();
    let path = "/s.jsonl";
    let _ = JsonlSessionStorage::create(fs.clone(), path, header(), clock.clone(), ids.clone())
        .await
        .unwrap();

    // A valid prefix: lane, then e1 → e2. The second e2 line is an actual
    // identical mutation, so it is safe to deduplicate.
    let exact_e2 = entry_mutation(3, "e2", Some("e1"));
    for mutation in [
        lane_mutation(1),
        entry_mutation(2, "e1", None),
        exact_e2.clone(),
        exact_e2.clone(),
        // Chains to e2, which remains the leaf after the exact duplicate is dropped.
        entry_mutation(4, "e3", Some("e2")),
        // Forward gap: the other writer's counter was ahead. Accepted.
        entry_mutation(10, "e4", Some("e3")),
    ] {
        fs.append_file(path, FileContent::Text(line(&mutation)), None)
            .await
            .unwrap();
    }

    let storage = JsonlSessionStorage::load(fs.clone(), path, clock.clone(), ids.clone())
        .await
        .expect("healed load");

    assert!(storage.get_entry("e1").await.unwrap().is_some());
    assert!(storage.get_entry("e2").await.unwrap().is_some());
    assert!(storage.get_entry("e3").await.unwrap().is_some());
    assert!(storage.get_entry("e4").await.unwrap().is_some());
    // The healed file retains one physical copy of the exact mutation.
    let content = fs.read_text_file(path, None).await.unwrap();
    let exact_count = content
        .lines()
        .skip(1)
        .filter_map(|raw| parse_mutation(raw).ok())
        .filter(|mutation| mutation == &exact_e2)
        .count();
    assert_eq!(exact_count, 1, "exact duplicate should be republished away");

    // Appends resume consistently after the highest surviving seq (10).
    let next = storage
        .append_entry(message_provisioned("e5", "e5"), "main")
        .await
        .unwrap();
    assert_eq!(
        next.seq(),
        11,
        "next append continues from the resynced seq"
    );
}

#[tokio::test]
async fn divergent_same_seq_is_rejected_without_rewriting_source() {
    let (fs, clock, ids) = fixture();
    let path = "/conflict.jsonl";
    let _ = JsonlSessionStorage::create(fs.clone(), path, header(), clock.clone(), ids.clone())
        .await
        .unwrap();
    let exact = entry_mutation(2, "same", None);
    for mutation in [
        lane_mutation(1),
        exact.clone(),
        exact,
        entry_mutation(2, "different", None),
    ] {
        fs.append_file(path, FileContent::Text(line(&mutation)), None)
            .await
            .unwrap();
    }
    let before = fs.read_text_file(path, None).await.unwrap();
    let result = JsonlSessionStorage::load(fs.clone(), path, clock, ids).await;
    assert!(
        result.is_err(),
        "different same-seq mutation must be rejected"
    );
    assert_eq!(
        fs.read_text_file(path, None).await.unwrap(),
        before,
        "conflict detection must not rewrite the source"
    );
}

#[tokio::test]
async fn entry_that_does_not_chain_to_the_leaf_is_re_chained() {
    let (fs, clock, ids) = fixture();
    let path = "/s.jsonl";
    let _ = JsonlSessionStorage::create(fs.clone(), path, header(), clock.clone(), ids.clone())
        .await
        .unwrap();
    for mutation in [
        lane_mutation(1),
        entry_mutation(2, "e1", None),
        // Parent is stale (another writer's leaf), not the current leaf `e1`.
        entry_mutation(3, "e2", None),
    ] {
        fs.append_file(path, FileContent::Text(line(&mutation)), None)
            .await
            .unwrap();
    }

    let storage = JsonlSessionStorage::load(fs.clone(), path, clock.clone(), ids)
        .await
        .expect("healed load");
    let e2 = storage.get_entry("e2").await.unwrap().unwrap();
    assert_eq!(
        e2.parent_id(),
        Some("e1"),
        "an unchained entry is re-parented onto the lane leaf"
    );
}
