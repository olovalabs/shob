use rpi_harness::session::types::{
    AssistantFrameOp, AssistantFrameRecord, LaneRecord, RecordBase, ToolReplay, ToolStartedRecord,
};
use serde_json::{json, Value};

fn base(id: &str) -> RecordBase {
    RecordBase {
        id: id.to_string(),
        seq: 123,
        lane: "main".to_string(),
        timestamp: 1_735_000_000_000,
    }
}

fn json_bytes(value: &Value) -> usize {
    serde_json::to_vec(value)
        .expect("audit value serializes")
        .len()
        + 1
}

fn compact_tool_started(mut value: Value) -> Value {
    let Value::Object(ref mut object) = value else {
        panic!("tool_started is an object");
    };
    object.remove("toolCallId");
    object.remove("toolName");
    value
}

fn compact_commit_stream(mut value: Value) -> Value {
    let Value::Object(ref mut object) = value else {
        panic!("assistant_frame is an object");
    };
    let entry_id = object
        .get("frame")
        .and_then(Value::as_object)
        .and_then(|frame| frame.get("entryId"))
        .cloned()
        .expect("commit stream has frame.entryId");
    object.remove("frame");
    object.insert("entryId".to_string(), entry_id);
    value
}

#[test]
fn record_field_size_audit() {
    let tool = LaneRecord::ToolStarted(ToolStartedRecord {
        base: base("tool-record-1"),
        run_id: "run-1234567890".to_string(),
        assistant_entry_id: "entry-assistant-1234567890".to_string(),
        tool_index: 0,
        tool_call_id: "toolu_1234567890".to_string(),
        tool_name: "bench_echo_0".to_string(),
        effective_args: json!({"text":"deterministic tool result\n".repeat(8)}),
        result_entry_id: "entry-tool-result-1234567890".to_string(),
        replay: ToolReplay::Never,
    });
    let tool_value = serde_json::to_value(&tool).expect("tool serializes");
    let tool_full = json_bytes(&tool_value);
    let tool_compact = json_bytes(&compact_tool_started(tool_value));

    let commit = LaneRecord::AssistantFrame(AssistantFrameRecord {
        base: base("frame-record-1"),
        run_id: "run-1234567890".to_string(),
        stream_index: 7,
        op: AssistantFrameOp::CommitStream,
        frame: Some(json!({"entryId":"entry-assistant-1234567890"})),
    });
    let commit_value = serde_json::to_value(&commit).expect("commit serializes");
    let commit_full = json_bytes(&commit_value);
    let commit_compact = json_bytes(&compact_commit_stream(commit_value));

    let append = LaneRecord::AssistantFrame(AssistantFrameRecord {
        base: base("frame-record-2"),
        run_id: "run-1234567890".to_string(),
        stream_index: 7,
        op: AssistantFrameOp::Append,
        frame: Some(json!({
            "kind":"content_delta",
            "text":"deterministic assistant content ".repeat(8),
            "index":0
        })),
    });
    let append_value = serde_json::to_value(&append).expect("append serializes");
    let append_full = json_bytes(&append_value);
    let append_payload = append_value
        .get("frame")
        .map(json_bytes)
        .expect("append has payload");

    assert!(tool_compact < tool_full);
    assert!(commit_compact < commit_full);
    assert!(append_payload < append_full);

    if std::env::var_os("RPI_RECORD_SIZE_AUDIT").is_some_and(|value| value != "0") {
        eprintln!(
            "RPI_RECORD_SIZE_AUDIT tool_started_full={} tool_started_without_tool_id_name={} tool_saving={} commit_stream_full={} commit_stream_top_level_entry_id={} commit_saving={} assistant_frame_append_full={} assistant_frame_payload_only={}",
            tool_full,
            tool_compact,
            tool_full - tool_compact,
            commit_full,
            commit_compact,
            commit_full - commit_compact,
            append_full,
            append_payload,
        );
    }

    // Keep the audit fixture intentionally JSON-object based: this verifies the
    // proposed wire-size deltas without changing the v4 codec or production
    // serialization path.
}
