//! Mirrors `packages/agent/src/harness/session/jsonl/codec.ts` — the v4 JSONL
//! codec: a header line followed by newline-terminated mutation lines.
//!
//! ## Wire shapes (all terminate with `\n`)
//!
//! - **Header**: `{"kind":"header","version":4,"id","createdAt","cwd",
//!   "parentSessionId"?,"legacyParentSessionPath"?,"metadata"?}` — exactly one,
//!   first line. `parentSessionId` and `legacyParentSessionPath` are mutually
//!   exclusive.
//! - **entry mutation**: `{"kind":"entry","lane"?,"<…flat entry fields…>"}`
//!   (`lane` present except on the fork path, which carries `lane: null`).
//! - **record mutation**: `{"kind":"record","<…lane record fields…>"}`.
//! - **lane mutation**: `{"kind":"lane","lane","leafId"?}`.
//! - **fact mutation**: `{"kind":"fact","fact":"name","name"?}` or
//!   `{"kind":"fact","fact":"label","targetId","label"?}`.
//!
//! ## Decode validation (mirrors `codec.ts`)
//!
//! version==4; positive safe-int `seq`; entry/record `type` whitelists;
//! `operation_started` `intent.kind` in {run,compaction,navigation};
//! `operation_finished` requires `runId`. Errors are `JsonlDecodeError` with
//! `kind: Syntax` (bad JSON) or `kind: Schema` (wrong shape/whitelist). The
//! storage layer keys torn-tail recovery off `kind: Syntax` on the last line.
//!
//! ## Adaptation
//!
//! TS spreads the decoded object into a typed `SessionMutation` with an `as
//! unknown` cast; Rust round-trips the parsed JSON through [`Entry`]'s manual
//! serde and [`LaneRecord`]'s `#[serde(tag="type")]` enum, stripping the two
//! wrapper keys (`kind`, and `lane` on entries) first. [`SessionMutation`]
//! already carries a full [`Entry`] (storage fields stamped by the caller), so
//! `parse_mutation` keeps the entry's `seq`/`parentId`/`timestamp` verbatim
//! from the wire — exactly as the TS port does.

use serde_json::Value;

use crate::session::jsonl::errors::{JsonlDecodeError, JsonlDecodeErrorKind};
use crate::session::jsonl::types::{JsonlSessionMetadata, JsonlSourceFormat, JsonlV4Header};
use crate::session::types::{JsonValue, LaneRecord, SessionMutation};

// ---- whitelists (mirror codec.ts ENTRY_TYPES / RECORD_TYPES / OPERATION_KINDS) ----

const ENTRY_TYPES: &[&str] = &[
    "message",
    "model_change",
    "thinking_level_change",
    "active_tools_change",
    "compaction",
    "branch_summary",
    "custom",
];
const RECORD_TYPES: &[&str] = &[
    "operation_started",
    "abort_requested",
    "operation_finished",
    "step_attempt",
    "tool_started",
    "queue_enqueued",
    "queue_cancelled",
    "write_deferred",
    "usage",
    "assistant_frame",
    "retry_pending",
];
const OPERATION_KINDS: &[&str] = &["run", "compaction", "navigation"];

// ============================ header ============================

/// Parse the first JSONL line into a validated [`JsonlV4Header`]. Mirrors TS
/// `parseHeader` -> `decodeHeader`.
pub fn parse_header(line: &str) -> Result<JsonlV4Header, JsonlDecodeError> {
    decode_header(line)
}

/// Serialize a header to its newline-terminated JSONL line. Mirrors TS
/// `encodeHeader`.
pub fn encode_header(header: &JsonlV4Header) -> String {
    // serde_json::to_string is infallible for this shape; the TS impl trusts
    // JSON.stringify the same way.
    let mut s = serde_json::to_string(header).expect("header serializes");
    s.push('\n');
    s
}

impl JsonlV4Header {
    /// Construct with the canonical v4 fields. `version` is forced to 4.
    pub fn new(
        id: String,
        created_at: i64,
        cwd: String,
        parent_session_id: Option<String>,
        legacy_parent_session_path: Option<String>,
        metadata: Option<serde_json::Map<String, JsonValue>>,
    ) -> Self {
        Self {
            kind: super::types::HeaderKind,
            version: 4,
            progress_format: None,
            id,
            created_at,
            cwd,
            parent_session_id,
            legacy_parent_session_path,
            metadata,
        }
    }
}

fn decode_header(line: &str) -> Result<JsonlV4Header, JsonlDecodeError> {
    let value = parse_object(line)?;
    let kind = value.get("kind").and_then(|v| v.as_str());
    if kind != Some("header") {
        return Err(schema("is not a header"));
    }
    let version = value
        .get("version")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| schema("has unsupported session version"))?;
    if version != 4 {
        return Err(schema("has unsupported session version"));
    }

    let parent_session_id = optional_string_field(&value, "parentSessionId")?;
    let legacy_parent_session_path = optional_string_field(&value, "legacyParentSessionPath")?;
    if parent_session_id.is_some() && legacy_parent_session_path.is_some() {
        return Err(schema(
            "has both parentSessionId and legacyParentSessionPath",
        ));
    }

    let metadata_value = value.get("metadata");
    let metadata = match metadata_value {
        None | Some(Value::Null) => None,
        Some(Value::Object(m)) => Some(m.clone()),
        _ => return Err(schema("has invalid metadata")),
    };

    let id = require_string(&value, "id")?;
    let created_at = require_timestamp(&value, "createdAt")?;
    let cwd = require_string(&value, "cwd")?;

    let progress_format = optional_string_field(&value, "progressFormat")?;
    if progress_format
        .as_deref()
        .is_some_and(|format| format != super::progress::FORMAT)
    {
        return Err(schema("unsupported progress format"));
    }
    Ok(JsonlV4Header {
        kind: super::types::HeaderKind,
        version: 4,
        progress_format,
        id,
        created_at,
        cwd,
        parent_session_id,
        legacy_parent_session_path,
        metadata,
    })
}

/// Mirrors TS `metadataFromHeader`: build the richness [`JsonlSessionMetadata`]
/// (path + filesystem mtime + source_format=4) from a header + the resolved
/// on-disk path + modification time.
pub fn metadata_from_header(
    header: &JsonlV4Header,
    path: &str,
    modified_at: i64,
) -> JsonlSessionMetadata {
    JsonlSessionMetadata {
        id: header.id.clone(),
        created_at: header.created_at,
        cwd: header.cwd.clone(),
        path: path.to_string(),
        modified_at,
        source_format: JsonlSourceFormat::V4,
        parent_session_id: header.parent_session_id.clone(),
        legacy_parent_session_path: header.legacy_parent_session_path.clone(),
        metadata: header.metadata.clone(),
    }
}

// ============================ mutations ============================

/// Parse a mutation line into a [`SessionMutation`]. Mirrors TS `parseMutation`
/// -> `decodeMutation`.
pub fn parse_mutation(line: &str) -> Result<SessionMutation, JsonlDecodeError> {
    let value = parse_object(line)?;
    let seq = require_sequence(&value)?;
    match value.get("kind").and_then(|v| v.as_str()) {
        Some("entry") => parse_entry_mutation(&value, seq),
        Some("record") => parse_record_mutation(&value, seq),
        Some("lane") => parse_lane_mutation(&value, seq),
        Some("fact") => parse_fact_mutation(&value, seq),
        _ => Err(schema("has unknown mutation kind")),
    }
}

fn mutation_value(mutation: &SessionMutation) -> Value {
    match mutation {
        SessionMutation::Entry { lane, entry, .. } => {
            // `{kind:"entry", lane, ...entry}` — emit an object with `kind`
            // first, then `lane` (present or null), then the entry's flat fields.
            let mut map = serde_json::Map::new();
            map.insert("kind".into(), Value::String("entry".into()));
            map.insert(
                "lane".into(),
                match lane {
                    Some(l) => Value::String(l.clone()),
                    None => Value::Null,
                },
            );
            if let Value::Object(entry_obj) = entry.to_flat_json() {
                for (k, v) in entry_obj {
                    map.insert(k, v);
                }
            }
            Value::Object(map)
        }
        SessionMutation::Record { record } => {
            // `{kind:"record", ...record}`.
            let rec = serde_json::to_value(record).expect("record serializes");
            let mut map = serde_json::Map::new();
            map.insert("kind".into(), Value::String("record".into()));
            if let Value::Object(rec_obj) = rec {
                for (k, v) in rec_obj {
                    map.insert(k, v);
                }
            }
            Value::Object(map)
        }
        SessionMutation::Lane { seq, lane, leaf_id } => {
            let mut map = serde_json::Map::new();
            map.insert("kind".into(), Value::String("lane".into()));
            map.insert("seq".into(), Value::Number((*seq).into()));
            map.insert("lane".into(), Value::String(lane.clone()));
            map.insert(
                "leafId".into(),
                match leaf_id {
                    Some(l) => Value::String(l.clone()),
                    None => Value::Null,
                },
            );
            Value::Object(map)
        }
        SessionMutation::FactName { seq, name } => {
            let mut map = serde_json::Map::new();
            map.insert("kind".into(), Value::String("fact".into()));
            map.insert("seq".into(), Value::Number((*seq).into()));
            map.insert("fact".into(), Value::String("name".into()));
            map.insert(
                "name".into(),
                name.as_ref()
                    .map_or(Value::Null, |name| Value::String(name.clone())),
            );
            Value::Object(map)
        }
        SessionMutation::FactLabel {
            seq,
            target_id,
            label,
        } => {
            let mut map = serde_json::Map::new();
            map.insert("kind".into(), Value::String("fact".into()));
            map.insert("seq".into(), Value::Number((*seq).into()));
            map.insert("fact".into(), Value::String("label".into()));
            map.insert("targetId".into(), Value::String(target_id.clone()));
            map.insert(
                "label".into(),
                label
                    .as_ref()
                    .map_or(Value::Null, |label| Value::String(label.clone())),
            );
            Value::Object(map)
        }
    }
}

/// Serialize a mutation into an existing JSONL byte buffer.
pub fn encode_mutation_into(mutation: &SessionMutation, output: &mut Vec<u8>) {
    serde_json::to_writer(&mut *output, &mutation_value(mutation)).expect("mutation serializes");
    output.push(b'\n');
}

/// Serialize a mutation to its newline-terminated JSONL line. Mirrors TS
/// `encodeMutation`.
pub fn encode_mutation(mutation: &SessionMutation) -> String {
    let mut output = Vec::new();
    encode_mutation_into(mutation, &mut output);
    String::from_utf8(output).expect("JSON is UTF-8")
}

// ---- per-kind decode (mirror parseEntryMutation / parseRecordMutation / ...) ----

fn parse_entry_mutation(
    value: &serde_json::Map<String, Value>,
    seq: u64,
) -> Result<SessionMutation, JsonlDecodeError> {
    // `lane` is optional (absent or null on the fork path).
    let lane = match value.get("lane") {
        None => None,
        Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        _ => return Err(schema("has invalid lane")),
    };

    // TS `requireString(value.type, "entry type")` — the *field* is `type`,
    // the validation *label* is "entry type".
    let entry_type = value
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| schema("has invalid entry type"))?;
    if !ENTRY_TYPES.contains(&entry_type) {
        return Err(schema(format!("has unknown entry type {entry_type}")));
    }

    // timestamp must be a non-negative integer (mirrors `requireTimestamp`).
    require_timestamp_map(value, "timestamp")?;

    // `custom` entries must carry `customType`.
    if entry_type == "custom" {
        if value.get("customType").and_then(|v| v.as_str()).is_none() {
            return Err(schema("has invalid customType"));
        }
    }

    // Rebuild the flat entry object (drop `kind` + `lane`) and let the Entry's
    // manual serde do the rest. `parentId` may be null (root entry) — preserved.
    let entry_obj = strip_wrapper_keys(value, &["kind", "lane"]);
    let entry: crate::session::types::Entry =
        serde_json::from_value(Value::Object(entry_obj)).map_err(|e| schema(e.to_string()))?;

    Ok(SessionMutation::Entry {
        seq,
        timestamp: entry.base().timestamp,
        lane,
        entry,
    })
}

fn parse_record_mutation(
    value: &serde_json::Map<String, Value>,
    seq: u64,
) -> Result<SessionMutation, JsonlDecodeError> {
    let record_type = value
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| schema("has invalid record type"))?;
    if !RECORD_TYPES.contains(&record_type) {
        return Err(schema(format!("has unknown record type {record_type}")));
    }
    // Validate the record-shape-specific checks before handing off to serde.
    if record_type == "operation_started" {
        let intent = value
            .get("intent")
            .ok_or_else(|| schema("has invalid intent"))?;
        let intent_obj = intent
            .as_object()
            .ok_or_else(|| schema("has invalid intent"))?;
        let op_kind = intent_obj
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| schema("has invalid operation kind"))?;
        if !OPERATION_KINDS.contains(&op_kind) {
            return Err(schema(format!("has unknown operation kind {op_kind}")));
        }
    }
    if record_type == "operation_finished" {
        if value.get("runId").and_then(|v| v.as_str()).is_none() {
            return Err(schema("has invalid runId"));
        }
    }

    // `{kind:"record", ...record}` → drop `kind`, keep the rest as the record.
    let record_obj = strip_wrapper_keys(value, &["kind"]);
    let record: LaneRecord =
        serde_json::from_value(Value::Object(record_obj)).map_err(|e| schema(e.to_string()))?;
    let _ = seq; // seq lives inside the record's base; not re-stamped by decode.
    Ok(SessionMutation::Record { record })
}

fn parse_lane_mutation(
    value: &serde_json::Map<String, Value>,
    seq: u64,
) -> Result<SessionMutation, JsonlDecodeError> {
    let lane = require_string_map(value, "lane")?;
    let leaf_id = match value.get("leafId") {
        None => None,
        Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        _ => return Err(schema("has invalid leafId")),
    };
    Ok(SessionMutation::Lane { seq, lane, leaf_id })
}

fn parse_fact_mutation(
    value: &serde_json::Map<String, Value>,
    seq: u64,
) -> Result<SessionMutation, JsonlDecodeError> {
    match value.get("fact").and_then(|v| v.as_str()) {
        Some("name") => {
            let name = match value.get("name") {
                None => None,
                Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()),
                _ => return Err(schema("has invalid name")),
            };
            Ok(SessionMutation::FactName { seq, name })
        }
        Some("label") => {
            let target_id = require_string_map(value, "targetId")?;
            let label = match value.get("label") {
                None => None,
                Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()),
                _ => return Err(schema("has invalid label")),
            };
            Ok(SessionMutation::FactLabel {
                seq,
                target_id,
                label,
            })
        }
        _ => Err(schema("has unknown fact type")),
    }
}

// ============================ shared field helpers ============================

fn parse_object(line: &str) -> Result<serde_json::Map<String, Value>, JsonlDecodeError> {
    let value: Value = serde_json::from_str(line).map_err(|_| syntax("is not valid JSON"))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(schema("is not a JSON object")),
    }
}

fn require_string(
    value: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<String, JsonlDecodeError> {
    value
        .get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| schema(format!("has invalid {field}")))
}

fn require_string_map(
    value: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<String, JsonlDecodeError> {
    require_string(value, field)
}

fn require_sequence(value: &serde_json::Map<String, Value>) -> Result<u64, JsonlDecodeError> {
    value
        .get("seq")
        .and_then(|v| v.as_u64())
        .filter(|&n| n > 0)
        .ok_or_else(|| schema("has invalid seq"))
}

fn require_timestamp(
    value: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<i64, JsonlDecodeError> {
    require_timestamp_map(value, field)
}

fn require_timestamp_map(
    value: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<i64, JsonlDecodeError> {
    let n = value
        .get(field)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| schema(format!("has invalid {field}")))?;
    if n < 0 {
        return Err(schema(format!("has invalid {field}")));
    }
    Ok(n)
}

fn optional_string_field(
    value: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<String>, JsonlDecodeError> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(schema(format!("has invalid {field}"))),
    }
}

/// Build a new object with the `wrapper` keys removed (mirrors the TS
/// `const { kind, lane, ...rest } = value` destructure).
fn strip_wrapper_keys(
    value: &serde_json::Map<String, Value>,
    wrapper: &[&str],
) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    for (k, v) in value {
        if !wrapper.contains(&k.as_str()) {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

fn syntax(message: impl Into<String>) -> JsonlDecodeError {
    JsonlDecodeError {
        kind: JsonlDecodeErrorKind::Syntax,
        message: message.into(),
    }
}

fn schema(message: impl Into<String>) -> JsonlDecodeError {
    JsonlDecodeError {
        kind: JsonlDecodeErrorKind::Schema,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::jsonl::types::HeaderKind;

    fn header() -> JsonlV4Header {
        JsonlV4Header::new(
            "sess-1".into(),
            1700_000_000_000,
            "/cwd".into(),
            None,
            None,
            None,
        )
    }

    #[test]
    fn header_round_trips() {
        let line = encode_header(&header());
        assert!(line.ends_with('\n'));
        let parsed = parse_header(line.trim_end()).unwrap();
        assert_eq!(parsed.version, 4);
        assert_eq!(parsed.id, "sess-1");
        assert_eq!(parsed.cwd, "/cwd");
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
    fn lane_mutation_round_trips() {
        let s = SessionMutation::Lane {
            seq: 2,
            lane: "main".into(),
            leaf_id: None,
        };
        let line = encode_mutation(&s);
        let parsed = parse_mutation(line.trim_end()).unwrap();
        match parsed {
            SessionMutation::Lane { seq, lane, leaf_id } => {
                assert_eq!(seq, 2);
                assert_eq!(lane, "main");
                assert_eq!(leaf_id, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn fact_name_round_trips() {
        let s = SessionMutation::FactName {
            seq: 3,
            name: Some("hello".into()),
        };
        let line = encode_mutation(&s);
        let parsed = parse_mutation(line.trim_end()).unwrap();
        match parsed {
            SessionMutation::FactName { seq, name } => {
                assert_eq!(seq, 3);
                assert_eq!(name.as_deref(), Some("hello"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn unknown_kind_is_schema_error() {
        let line = r#"{"kind":"bogus","seq":1}"#;
        let err = parse_mutation(line).unwrap_err();
        assert_eq!(err.kind, JsonlDecodeErrorKind::Schema);
    }

    #[test]
    fn bad_json_is_syntax_error() {
        let line = "{not json";
        let err = parse_mutation(line).unwrap_err();
        assert_eq!(err.kind, JsonlDecodeErrorKind::Syntax);
    }

    #[test]
    fn header_kind_marker_serializes_as_string() {
        let mut map = serde_json::Map::new();
        map.insert("kind".into(), serde_json::to_value(HeaderKind).unwrap());
        let v = serde_json::to_string(&serde_json::Value::Object(map)).unwrap();
        assert!(v.contains("\"kind\":\"header\""));
    }
}
