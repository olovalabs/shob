//! M5c integration — JSONL v4 codec round-trips + decode error classification.
//! Mirrors `packages/agent/test/harness/session/jsonl-codec.test.ts`.
//!
//! Exercises the public codec surface (`encode_header`/`parse_header`,
//! `encode_mutation`/`parse_mutation`, `metadata_from_header`) against the
//! rust wire shapes. Run against `rpi_harness::session::jsonl::*` only — no FS.

use std::collections::BTreeMap;

use rpi_agent::message::AgentMessage;
use rpi_harness::session::jsonl::{
    encode_header, encode_mutation, metadata_from_header, parse_header, parse_mutation, HeaderKind,
    JsonlDecodeErrorKind, JsonlSessionMetadata, JsonlSourceFormat, JsonlV4Header,
};
use rpi_harness::session::types::{
    AssistantFrameOp, AssistantFrameRecord, EntryBase, LaneRecord, OperationIntent,
    OperationStartedRecord, RecordBase, SessionMutation,
};

fn header_with_parent() -> JsonlV4Header {
    let mut metadata = serde_json::Map::new();
    metadata.insert("owner".into(), serde_json::Value::String("agent".into()));
    metadata.insert("nested".into(), serde_json::json!({ "enabled": true }));
    metadata.insert("values".into(), serde_json::json!([1, null, "two"]));
    JsonlV4Header::new(
        "session".into(),
        1_700_000_000_000,
        "/workspace/project".into(),
        Some("parent".into()),
        None,
        Some(metadata),
    )
}

fn header_with_legacy_parent() -> JsonlV4Header {
    JsonlV4Header::new(
        "legacy-child".into(),
        1_700_000_000_001,
        "/workspace/project".into(),
        None,
        Some("/sessions/missing-parent.jsonl".into()),
        None,
    )
}

fn header_with_metadata_map() -> JsonlV4Header {
    let mut metadata = serde_json::Map::new();
    metadata.insert("owner".into(), serde_json::Value::String("agent".into()));
    JsonlV4Header::new(
        "session".into(),
        1_700_000_000_000,
        "/workspace/project".into(),
        None,
        Some("/sessions/missing-parent.jsonl".into()),
        Some(metadata),
    )
}

fn custom_entry(
    id: &str,
    seq: u64,
    parent_id: Option<&str>,
    timestamp: i64,
) -> rpi_harness::session::types::Entry {
    use rpi_harness::session::types::{CustomEntry, Entry};
    let base = EntryBase {
        entry_type: "custom".to_string(),
        id: id.to_string(),
        seq,
        parent_id: parent_id.map(|s| s.to_string()),
        timestamp,
    };
    Entry::Custom(CustomEntry {
        base,
        custom_type: "note".to_string(),
        data: Some(serde_json::json!({ "text": "hello" })),
    })
}

fn lane_bound_entry_mutation(seq: u64, lane: &str) -> SessionMutation {
    SessionMutation::Entry {
        seq,
        timestamp: 100,
        lane: Some(lane.to_string()),
        entry: custom_entry("entry-1", seq, None, 100),
    }
}

fn imported_entry_mutation_no_lane(seq: u64) -> SessionMutation {
    SessionMutation::Entry {
        seq,
        timestamp: 100,
        lane: None,
        entry: custom_entry("entry-1", seq, None, 100),
    }
}

fn operation_started_record(seq: u64, lane: &str, id: &str) -> LaneRecord {
    LaneRecord::OperationStarted(OperationStartedRecord {
        base: RecordBase {
            id: id.to_string(),
            seq,
            lane: lane.to_string(),
            timestamp: 100,
        },
        source_leaf_id: None,
        intent: OperationIntent::Run {
            original_prompt: Vec::new(),
            initial_messages: Vec::new(),
            system_prompt_override: None,
            resume_data: None,
        },
    })
}

fn record_mutation(record: LaneRecord) -> SessionMutation {
    SessionMutation::Record { record }
}

fn lane_mutation(seq: u64, lane: &str, leaf_id: Option<&str>) -> SessionMutation {
    SessionMutation::Lane {
        seq,
        lane: lane.to_string(),
        leaf_id: leaf_id.map(|s| s.to_string()),
    }
}

fn assert_header_round_trip(header: &JsonlV4Header) {
    let encoded = encode_header(header);
    assert!(encoded.ends_with('\n'), "header line must end with newline");
    let parsed = parse_header(encoded.trim_end()).expect("header parses");
    assert_eq!(parsed.version, 4);
    assert_eq!(parsed.id, header.id);
    assert_eq!(parsed.created_at, header.created_at);
    assert_eq!(parsed.cwd, header.cwd);
    assert_eq!(parsed.parent_session_id, header.parent_session_id);
    assert_eq!(
        parsed.legacy_parent_session_path,
        header.legacy_parent_session_path
    );
    assert_eq!(parsed.metadata, header.metadata);
}

fn assert_mutation_round_trip(mutation: &SessionMutation) {
    let encoded = encode_mutation(mutation);
    assert!(
        encoded.ends_with('\n'),
        "mutation line must end with newline"
    );
    let parsed = parse_mutation(encoded.trim_end()).expect("mutation parses");
    assert_eq!(parsed.seq(), mutation.seq());
    // Round-trip equality holds for SessionMutation (Entry/Record/Lane/Fact).
    assert_eq!(&parsed, mutation);
}

fn user_message(text: &str) -> AgentMessage {
    AgentMessage::User(rpi_ai::types::UserMessage::new(text, 1))
}

#[test]
fn header_round_trips_every_field_with_resolved_parent() {
    assert_header_round_trip(&header_with_parent());
}

#[test]
fn header_round_trips_unresolved_legacy_parent_path() {
    assert_header_round_trip(&header_with_legacy_parent());
}

#[test]
fn header_rejects_version_3() {
    let line = r#"{"kind":"header","version":3,"id":"x","createdAt":0,"cwd":"."}"#;
    assert!(parse_header(line).is_err());
}

#[test]
fn header_rejects_both_parents() {
    let line = r#"{"kind":"header","version":4,"id":"x","createdAt":0,"cwd":".",
        "parentSessionId":"p","legacyParentSessionPath":"lp"}"#;
    assert!(parse_header(line).is_err());
}

#[test]
fn header_kind_marker_serializes_as_literal_header() {
    let mut map = serde_json::Map::new();
    map.insert("kind".into(), serde_json::to_value(HeaderKind).unwrap());
    let v = serde_json::to_string(&serde_json::Value::Object(map)).unwrap();
    assert!(v.contains("\"kind\":\"header\""));
}

#[test]
fn metadata_from_header_projects_header_and_fs_fields() {
    let header = header_with_metadata_map();
    let metadata = metadata_from_header(&header, "/sessions/session.jsonl", 1_700_000_000_100);
    assert_eq!(metadata.id, "session");
    assert_eq!(metadata.created_at, 1_700_000_000_000);
    assert_eq!(metadata.cwd, "/workspace/project");
    assert_eq!(metadata.path, "/sessions/session.jsonl");
    assert_eq!(metadata.modified_at, 1_700_000_000_100);
    assert_eq!(metadata.source_format, JsonlSourceFormat::V4);
    assert_eq!(
        metadata.legacy_parent_session_path.as_deref(),
        Some("/sessions/missing-parent.jsonl")
    );
    let owner = metadata
        .metadata
        .as_ref()
        .and_then(|m| m.get("owner"))
        .and_then(|v| v.as_str());
    assert_eq!(owner, Some("agent"));
}

#[test]
fn parse_header_missing_value_fields_is_schema_error() {
    // No createdAt — schema error, not syntax.
    let line = r#"{"kind":"header","version":4,"id":"x","cwd":"."}"#;
    let err = parse_header(line).unwrap_err();
    assert_eq!(err.kind, JsonlDecodeErrorKind::Schema);
}

#[test]
fn mutation_returns_syntax_and_schema_errors() {
    // Syntax: not valid JSON.
    let syntax = parse_mutation("{");
    assert!(syntax.is_err());
    assert_eq!(syntax.unwrap_err().kind, JsonlDecodeErrorKind::Syntax);
    // Schema: valid JSON, unknown kind.
    let schema = parse_mutation(r#"{"kind":"unknown","seq":1}"#);
    assert!(schema.is_err());
    assert_eq!(schema.unwrap_err().kind, JsonlDecodeErrorKind::Schema);
}

#[test]
fn mutation_round_trips_lane_bound_entry_line() {
    assert_mutation_round_trip(&lane_bound_entry_mutation(1, "main"));
}

#[test]
fn mutation_round_trips_imported_entry_line_without_lane() {
    assert_mutation_round_trip(&imported_entry_mutation_no_lane(1));
}

#[test]
fn mutation_round_trips_record_line() {
    let record = operation_started_record(1, "main", "run-1");
    assert_mutation_round_trip(&record_mutation(record));
}

/// Frame progress must survive a JSONL round trip, because that is the storage
/// a real crashed session is recovered from — the in-memory tests would not
/// catch a missing codec whitelist entry.
#[test]
fn mutation_round_trips_assistant_frame_record() {
    let record = LaneRecord::AssistantFrame(AssistantFrameRecord {
        base: RecordBase {
            id: "frame-1".into(),
            seq: 1,
            lane: "main".into(),
            timestamp: 7,
        },
        run_id: "run-1".into(),
        stream_index: 0,
        op: AssistantFrameOp::Append,
        frame: Some(serde_json::json!({"type": "text_delta", "contentIndex": 0, "delta": "hi"})),
    });
    let line = encode_mutation(&record_mutation(record.clone()));
    // Must be the camelCase wire form the codec whitelist expects.
    assert!(
        line.contains(r#""type":"assistant_frame""#),
        "record line must carry its record type tag: {line}"
    );
    assert!(
        line.contains(r#""runId":"run-1""#) && line.contains(r#""streamIndex":0"#),
        "record fields must be camelCase like every sibling record: {line}"
    );
    assert_mutation_round_trip(&record_mutation(record));
}

/// The `ClearRun` marker is what retires a committed run's frames.
#[test]
fn mutation_round_trips_assistant_frame_clear_run_record() {
    let record = LaneRecord::AssistantFrame(AssistantFrameRecord {
        base: RecordBase {
            id: "frame-2".into(),
            seq: 2,
            lane: "main".into(),
            timestamp: 8,
        },
        run_id: "run-1".into(),
        stream_index: 0,
        op: AssistantFrameOp::ClearRun,
        frame: None,
    });
    let line = encode_mutation(&record_mutation(record.clone()));
    assert!(
        line.contains(r#""op":"clear_run""#) && !line.contains(r#""frame""#),
        "a clear marker carries no frame payload: {line}"
    );
    assert_mutation_round_trip(&record_mutation(record));
}

#[test]
fn mutation_round_trips_lane_line() {
    assert_mutation_round_trip(&lane_mutation(1, "thread", Some("entry-1")));
}

#[test]
fn mutation_round_trips_lane_line_with_null_leaf() {
    assert_mutation_round_trip(&lane_mutation(1, "thread", None));
}

#[test]
fn mutation_round_trips_fact_name_lines_including_cleared() {
    assert_mutation_round_trip(&SessionMutation::FactName {
        seq: 1,
        name: Some("Example".into()),
    });
    assert_mutation_round_trip(&SessionMutation::FactName { seq: 2, name: None });
    assert_mutation_round_trip(&SessionMutation::FactLabel {
        seq: 3,
        target_id: "entry-1".into(),
        label: Some("checkpoint".into()),
    });
    assert_mutation_round_trip(&SessionMutation::FactLabel {
        seq: 4,
        target_id: "entry-1".into(),
        label: None,
    });
}

#[test]
fn mutation_rejects_custom_entry_without_custom_type() {
    // {kind:"entry", type:"custom", id, parentId:null, seq, timestamp} — no customType.
    let line =
        r#"{"kind":"entry","type":"custom","id":"entry","parentId":null,"seq":1,"timestamp":1}"#;
    let err = parse_mutation(line).unwrap_err();
    assert_eq!(err.kind, JsonlDecodeErrorKind::Schema);
}

#[test]
fn mutation_rejects_operation_started_without_intent() {
    let line = r#"{"kind":"record","type":"operation_started","id":"run","lane":"main","seq":1,"timestamp":1,"sourceLeafId":null}"#;
    let err = parse_mutation(line).unwrap_err();
    assert_eq!(err.kind, JsonlDecodeErrorKind::Schema);
}

#[test]
fn mutation_rejects_operation_started_with_unknown_intent_kind() {
    let line = r#"{"kind":"record","type":"operation_started","id":"run","lane":"main","seq":1,"timestamp":1,"sourceLeafId":null,"intent":{"kind":"bogus"}}"#;
    let err = parse_mutation(line).unwrap_err();
    assert_eq!(err.kind, JsonlDecodeErrorKind::Schema);
}

#[test]
fn mutation_rejects_operation_finished_without_run_id() {
    let line = r#"{"kind":"record","type":"operation_finished","id":"finish","lane":"main","seq":1,"timestamp":1,"outcome":"completed"}"#;
    let err = parse_mutation(line).unwrap_err();
    assert_eq!(err.kind, JsonlDecodeErrorKind::Schema);
}

#[test]
fn mutation_rejects_unknown_entry_type() {
    let line = r#"{"kind":"entry","type":"bogus","id":"e","parentId":null,"seq":1,"timestamp":1}"#;
    let err = parse_mutation(line).unwrap_err();
    assert_eq!(err.kind, JsonlDecodeErrorKind::Schema);
}

#[test]
fn mutation_rejects_non_positive_seq() {
    let line = r#"{"kind":"lane","seq":0,"lane":"main","leafId":null}"#;
    let err = parse_mutation(line).unwrap_err();
    assert_eq!(err.kind, JsonlDecodeErrorKind::Schema);
}

#[test]
fn metadata_is_jsonl_source_format_v4() {
    let header = header_with_parent();
    let metadata = metadata_from_header(&header, "/p.jsonl", 5);
    assert_eq!(metadata.source_format, JsonlSourceFormat::V4);
    // to_base projects id/created_at/parent_session_id.
    let base = metadata.to_base();
    assert_eq!(base.id, "session");
    assert_eq!(base.created_at, 1_700_000_000_000);
    assert_eq!(base.parent_session_id.as_deref(), Some("parent"));
}

#[test]
fn unused_helper_suppresses_dead_code() {
    // user_message is retained for parity with the TS fixtures; keep it referenced
    // so it does not warn under the test build.
    let _ = user_message("anchor");
    let _m: BTreeMap<String, String> = BTreeMap::new();
    let _ = JsonlSessionMetadata {
        id: String::new(),
        created_at: 0,
        cwd: String::new(),
        path: String::new(),
        modified_at: 0,
        source_format: JsonlSourceFormat::V4,
        parent_session_id: None,
        legacy_parent_session_path: None,
        metadata: None,
    };
}
