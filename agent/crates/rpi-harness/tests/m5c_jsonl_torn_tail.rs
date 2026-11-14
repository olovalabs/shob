//! M5c integration — torn-tail recovery + hard-corruption classification for
//! `JsonlSessionStorage::load`. Mirrors the torn-tail / corruption slice of
//! `packages/agent/test/harness/session/jsonl.test.ts` (the "truncates a
//! malformed final line", "rejects a complete invalid final mutation",
//! "rejects a malformed middle line", "repairs a valid final line missing its
//! newline" cases) run against the in-memory FS.
//!
//! Invariants exercised (plan §5.14):
//! - Only a *syntax* error on the *last* physical line is a recoverable torn
//!   tail: atomically publish the valid prefix, then proceed.
//! - A *schema* error on the last line, OR any error on a non-last line, is hard
//!   corruption → `invalid_entry`, file untouched.

use std::sync::Arc;

use rpi_agent::message::AgentMessage;
use rpi_harness::error::SessionErrorCode;
use rpi_harness::session::jsonl::{encode_mutation, JsonlSessionStorage, JsonlV4Header};
use rpi_harness::session::memory::{CounterIdGenerator, FakeClock};
use rpi_harness::session::types::{
    EntryOrder, EntryQuery, LaneRecord, OperationIntent, OperationStartedRecord, ProvisionedEntry,
    ProvisionedKind, RecordBase, SessionMutation, SessionStorage,
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

async fn create_simple(fs: &Arc<dyn FileSystem>, path: &str) -> JsonlSessionStorage {
    JsonlSessionStorage::create(
        fs.clone(),
        path,
        header(),
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    )
    .await
    .unwrap()
}

fn lane_mutation(seq: u64, lane: &str) -> SessionMutation {
    SessionMutation::Lane {
        seq,
        lane: lane.to_string(),
        leaf_id: None,
    }
}

fn newest_first() -> EntryQuery {
    EntryQuery {
        order: Some(EntryOrder::NewestFirst),
        ..Default::default()
    }
}

fn oldest_first() -> EntryQuery {
    EntryQuery {
        order: Some(EntryOrder::OldestFirst),
        ..Default::default()
    }
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

#[tokio::test]
async fn torn_tail_syntax_error_on_last_line_is_repaired() {
    let (fs, clock, ids) = fixture();
    let path = "/s.jsonl";
    let _ = create_simple(&fs, path).await;
    // Append a valid first mutation (seq=1) + a garbage partial last line.
    let valid = encode_mutation(&lane_mutation(1, "main"));
    fs.append_file(path, FileContent::Text(valid), None)
        .await
        .unwrap();
    fs.append_file(path, FileContent::Text("{not json".into()), None)
        .await
        .unwrap();

    let reloaded = JsonlSessionStorage::load(fs.clone(), path, clock, ids)
        .await
        .unwrap();
    // The torn tail was dropped; the file ends after the valid lane line.
    let content = fs.read_text_file(path, None).await.unwrap();
    assert!(!content.contains("{not json"), "torn line must be removed");
    assert!(content.contains("\"kind\":\"lane\""));
    // Reloaded storage is usable for further appends.
    let _ = reloaded.get_name().await.unwrap();
}

#[tokio::test]
async fn torn_tail_partial_json_object_on_last_line_is_repaired() {
    // Mirrors the TS "truncates a malformed final line" case: a partial
    // `{"kind":"entry"` (no closing brace) is a *syntax* error on the last line.
    let (fs, clock, ids) = fixture();
    let path = "/s.jsonl";
    let storage = create_simple(&fs, path).await;
    storage
        .append_entry(message_provisioned("note", "kept"), "main")
        .await
        .unwrap();
    let valid_prefix = fs.read_text_file(path, None).await.unwrap();
    fs.append_file(path, FileContent::Text("{\"kind\":\"entry\"".into()), None)
        .await
        .unwrap();

    let reopened = JsonlSessionStorage::load(fs.clone(), path, clock, ids)
        .await
        .unwrap();
    let listed = reopened.find_entries(&newest_first()).await.unwrap();
    assert_eq!(listed.len(), 1, "torn append must not survive");
    // File content is back to the valid prefix.
    assert_eq!(fs.read_text_file(path, None).await.unwrap(), valid_prefix);
    // A further append lands on its own line at seq=2.
    let after = reopened
        .append_entry(message_provisioned("after", "after"), "main")
        .await
        .unwrap();
    assert_eq!(after.seq(), 2);
}

#[tokio::test]
async fn schema_error_on_last_line_is_hard_corruption() {
    // Valid JSON but unknown kind → schema error on the last line is NOT a torn
    // tail; the file is left untouched and load fails with invalid_entry.
    let (fs, clock, ids) = fixture();
    let path = "/s.jsonl";
    let _ = create_simple(&fs, path).await;
    fs.append_file(
        path,
        FileContent::Text("{\"kind\":\"bogus\",\"seq\":1}\n".into()),
        None,
    )
    .await
    .unwrap();
    let corrupted = fs.read_text_file(path, None).await.unwrap();

    let err = JsonlSessionStorage::load(fs.clone(), path, clock, ids)
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, SessionErrorCode::InvalidEntry);
    assert_eq!(fs.read_text_file(path, None).await.unwrap(), corrupted);
}

#[tokio::test]
async fn syntax_error_on_non_last_line_is_hard_corruption() {
    // Garbage on line 2 (NOT last), then a valid line 3 → hard corruption.
    let (fs, clock, ids) = fixture();
    let path = "/s.jsonl";
    let _ = create_simple(&fs, path).await;
    fs.append_file(path, FileContent::Text("{not json\n".into()), None)
        .await
        .unwrap();
    fs.append_file(
        path,
        FileContent::Text(encode_mutation(&lane_mutation(1, "main"))),
        None,
    )
    .await
    .unwrap();
    let corrupted = fs.read_text_file(path, None).await.unwrap();

    let err = JsonlSessionStorage::load(fs.clone(), path, clock, ids)
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, SessionErrorCode::InvalidEntry);
    assert_eq!(fs.read_text_file(path, None).await.unwrap(), corrupted);
}

#[tokio::test]
async fn schema_error_on_non_last_line_is_hard_corruption() {
    // A complete-but-invalid mutation (unknown kind, valid JSON) on a non-last
    // line is hard corruption.
    let (fs, clock, ids) = fixture();
    let path = "/s.jsonl";
    let _ = create_simple(&fs, path).await;
    fs.append_file(
        path,
        FileContent::Text("{\"kind\":\"bogus\",\"seq\":1}\n".into()),
        None,
    )
    .await
    .unwrap();
    fs.append_file(
        path,
        FileContent::Text(encode_mutation(&lane_mutation(2, "main"))),
        None,
    )
    .await
    .unwrap();

    let err = JsonlSessionStorage::load(fs.clone(), path, clock, ids)
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, SessionErrorCode::InvalidEntry);
}

#[tokio::test]
async fn missing_trailing_newline_is_repaired_on_load() {
    // Mirrors the TS "repairs a valid final line missing its newline": a valid
    // file that simply lacks the trailing "\n" is repaired (a "\n" appended)
    // and load succeeds; a subsequent append lands on its own line.
    let (fs, clock, ids) = fixture();
    let path = "/s.jsonl";
    let storage = create_simple(&fs, path).await;
    storage
        .append_entry(message_provisioned("first", "first"), "main")
        .await
        .unwrap();
    let unterminated = fs
        .read_text_file(path, None)
        .await
        .unwrap()
        .trim_end()
        .to_string();
    fs.write_file(path, FileContent::Text(unterminated.clone()), None)
        .await
        .unwrap();

    let reopened = JsonlSessionStorage::load(fs.clone(), path, clock.clone(), ids.clone())
        .await
        .unwrap();
    let content = fs.read_text_file(path, None).await.unwrap();
    assert_eq!(content, format!("{unterminated}\n"));
    let second = reopened
        .append_entry(message_provisioned("second", "second"), "main")
        .await
        .unwrap();
    assert_eq!(second.seq(), 2);

    let verified = JsonlSessionStorage::load(fs, path, clock, ids)
        .await
        .unwrap();
    let listed = verified.find_entries(&oldest_first()).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id(), "first");
    assert_eq!(listed[1].id(), "second");
}

#[tokio::test]
async fn empty_file_is_invalid_entry() {
    let (fs, clock, ids) = fixture();
    let path = "/s.jsonl";
    fs.write_file(path, FileContent::Text(String::new()), None)
        .await
        .unwrap();
    let err = JsonlSessionStorage::load(fs, path, clock, ids)
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, SessionErrorCode::InvalidEntry);
}

#[tokio::test]
async fn header_only_round_trips_and_is_appendable() {
    // A freshly-created session (header only) loads cleanly and accepts a record.
    let (fs, clock, ids) = fixture();
    let path = "/s.jsonl";
    let _ = JsonlSessionStorage::create(fs.clone(), path, header(), clock.clone(), ids.clone())
        .await
        .unwrap();
    let reopened = JsonlSessionStorage::load(fs.clone(), path, clock, ids)
        .await
        .unwrap();
    let started = LaneRecord::OperationStarted(OperationStartedRecord {
        base: RecordBase {
            id: "run-1".into(),
            seq: 0,
            lane: "main".into(),
            timestamp: 0,
        },
        source_leaf_id: None,
        intent: OperationIntent::Run {
            original_prompt: Vec::new(),
            initial_messages: Vec::new(),
            system_prompt_override: None,
            resume_data: None,
        },
    });
    let appended = reopened.append_record(started).await.unwrap();
    assert_eq!(appended.id(), "run-1");
    assert_eq!(appended.seq(), 1);
}
