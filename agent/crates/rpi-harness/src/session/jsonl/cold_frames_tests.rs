use super::*;
use crate::session::jsonl::encode_mutation;
use crate::session::state::SessionState;
use crate::session::types::{OperationIntent, OperationStartedRecord, QueueKind, RecordBase};
use async_trait::async_trait;
use rpi_tools::{env::ReadFile, FileError};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

struct MemoryReader {
    bytes: Mutex<Vec<u8>>,
    reads: AtomicUsize,
    read_bytes: AtomicUsize,
}
#[async_trait]
impl ReadFile for MemoryReader {
    async fn read_all(&self) -> Result<Vec<u8>, FileError> {
        Ok(self.bytes.lock().unwrap().clone())
    }
    async fn read_range(&self, offset: u64, length: usize) -> Result<Vec<u8>, FileError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.read_bytes.fetch_add(length, Ordering::Relaxed);
        let bytes = self.bytes.lock().unwrap();
        Ok(bytes
            .get(offset as usize..offset as usize + length)
            .unwrap_or_default()
            .to_vec())
    }
}
fn frame(seq: u64, lane: &str, run: &str, op: AssistantFrameOp) -> AssistantFrameRecord {
    AssistantFrameRecord {
        base: RecordBase {
            id: format!("frame-{seq}"),
            seq,
            timestamp: 1,
            lane: lane.into(),
        },
        run_id: run.into(),
        stream_index: (seq % 2) as usize,
        op,
        frame: Some(json!({"type":"text_delta","delta":"🌊\r\n".repeat(32)})),
    }
}
fn fixture() -> (ResidentState, SessionState, Arc<MemoryReader>) {
    let mut lazy = ResidentState::default();
    let mut eager = SessionState::new();
    let reader = Arc::new(MemoryReader {
        bytes: Mutex::new(Vec::new()),
        reads: AtomicUsize::new(0),
        read_bytes: AtomicUsize::new(0),
    });
    let handle: Arc<dyn ReadFile> = reader.clone();
    for seq in 1..=100 {
        let mutation = if seq == 1 {
            SessionMutation::Lane {
                seq,
                lane: "side".into(),
                leaf_id: None,
            }
        } else if seq % 9 == 0 {
            SessionMutation::FactName {
                seq,
                name: Some(format!("name-{seq}")),
            }
        } else {
            let lane = if seq % 2 == 0 { "main" } else { "side" };
            let run = if seq % 3 == 0 { "run-b" } else { "run-a" };
            let op = if seq % 11 == 0 {
                AssistantFrameOp::CommitStream
            } else if seq % 7 == 0 {
                AssistantFrameOp::ClearRun
            } else {
                AssistantFrameOp::Append
            };
            let mut frame = frame(seq, lane, run, op);
            if seq % 13 == 0 {
                frame.frame = None;
            }
            SessionMutation::Record {
                record: LaneRecord::AssistantFrame(frame),
            }
        };
        let line = encode_mutation(&mutation);
        let offset = reader.bytes.lock().unwrap().len();
        reader
            .bytes
            .lock()
            .unwrap()
            .extend_from_slice(line.as_bytes());
        match &mutation {
            SessionMutation::Record {
                record: LaneRecord::AssistantFrame(frame),
            } if frame.op != AssistantFrameOp::CommitStream => {
                lazy.defer(frame, &handle, offset as u64, &line).unwrap();
            }
            _ => {
                lazy.apply_mutation(mutation.clone()).unwrap();
            }
        }
        eager.apply_mutation(mutation).unwrap();
    }
    (lazy, eager, reader)
}

#[tokio::test]
async fn cold_queries_preserve_eager_filters_order_bounds_and_defensive_copies() {
    let (lazy, eager, reader) = fixture();
    assert!(lazy.cold.locations.len() > 70);
    assert!(
        lazy.reducer
            .find_records(&RecordQuery::default())
            .unwrap()
            .len()
            < 10
    );
    assert_eq!(lazy.sequence(), eager.sequence());
    assert!(lazy.validate_unused_id("frame-2").is_err());
    for order in [
        None,
        Some(EntryOrder::OldestFirst),
        Some(EntryOrder::NewestFirst),
    ] {
        for limit in [None, Some(1), Some(8), Some(1000)] {
            for lane in [None, Some("main".into()), Some("side".into())] {
                for run_id in [None, Some("run-a".into()), Some("missing".into())] {
                    let query = RecordQuery {
                        order,
                        limit,
                        lane: lane.clone(),
                        run_id,
                        after_seq: Some(12),
                        ..Default::default()
                    };
                    assert_eq!(
                        lazy.record_plan(&query).unwrap().read().await.unwrap(),
                        eager.find_records(&query).unwrap()
                    );
                }
            }
        }
    }
    for query in [
        RecordQuery {
            record_type: Some("usage"),
            ..Default::default()
        },
        RecordQuery {
            operation_kind: Some("run"),
            ..Default::default()
        },
        RecordQuery {
            queue_kind: Some(QueueKind::Context),
            ..Default::default()
        },
        RecordQuery {
            tool_call_id: Some("tool".into()),
            ..Default::default()
        },
        RecordQuery {
            target_entry_id: Some("target".into()),
            ..Default::default()
        },
        RecordQuery {
            pending_queue_only: true,
            ..Default::default()
        },
    ] {
        let before = reader.reads.load(Ordering::Relaxed);
        assert_eq!(
            lazy.record_plan(&query).unwrap().read().await.unwrap(),
            eager.find_records(&query).unwrap()
        );
        assert_eq!(reader.reads.load(Ordering::Relaxed), before);
    }
    let query = RecordQuery {
        limit: Some(1),
        ..Default::default()
    };
    let mut first = lazy.record_plan(&query).unwrap().read().await.unwrap();
    if let LaneRecord::AssistantFrame(frame) = &mut first[0] {
        frame.frame = Some(json!("mutated"));
    }
    assert_eq!(
        lazy.record_plan(&query).unwrap().read().await.unwrap(),
        eager.find_records(&query).unwrap()
    );
    assert!(lazy
        .record_plan(&RecordQuery {
            limit: Some(0),
            ..Default::default()
        })
        .is_err());
}

#[tokio::test]
async fn cold_record_pages_and_interleaved_log_match_eager_state() {
    let (lazy, eager, _) = fixture();
    for lane in ["main", "side"] {
        for from_end in [false, true] {
            for limit in [1, 7, 1000] {
                let query = RecordPageQuery {
                    lane: lane.into(),
                    after_seq: Some(10),
                    before_seq: Some(85),
                    limit,
                    from_end,
                };
                let (plan, total) = lazy.page_plan(&query).unwrap();
                let actual = plan.page(total).await.unwrap();
                let expected = eager.record_page(&query).unwrap();
                assert_eq!(actual.total, expected.total);
                assert_eq!(actual.records, expected.records);
            }
        }
    }
    for after_seq in [None, Some(9), Some(99), Some(1000)] {
        for limit in [None, Some(1), Some(13), Some(1000)] {
            let options = LogOptions { after_seq, limit };
            assert_eq!(
                lazy.log_plan(&options).unwrap().read().await.unwrap(),
                eager.get_log(&options).unwrap()
            );
        }
    }
}

#[tokio::test]
async fn cold_bounded_reads_coalesce_and_do_not_warm_or_copy_the_index() {
    let (mut lazy, _, reader) = fixture();
    let count = lazy.cold.locations.len();
    let query = RecordQuery {
        record_type: Some("assistant_frame"),
        limit: Some(5),
        ..Default::default()
    };
    let plan = lazy.record_plan(&query).unwrap();
    // Reconcile can replace the entire resident view while a query owns the
    // prior source generation. Dropping the view does not invalidate the plan.
    lazy = ResidentState::default();
    let records = plan.read().await.unwrap();
    assert_eq!(records.len(), 5);
    assert!(reader.reads.load(Ordering::Relaxed) <= 2);
    assert!(reader.read_bytes.load(Ordering::Relaxed) < reader.bytes.lock().unwrap().len() / 4);
    assert!(count > 70);
    assert!(lazy.cold.locations.is_empty());
}

#[tokio::test]
async fn cold_retirement_matches_eager_state_and_keeps_inflight_plans_and_ids() {
    for op in [AssistantFrameOp::ClearStream, AssistantFrameOp::ClearRun] {
        let (mut lazy, mut eager, _) = fixture();
        let query = RecordQuery::default();
        let before = eager.find_records(&query).unwrap();
        let plan = lazy.record_plan(&query).unwrap();
        let mut clear = frame(101, "main", "run-a", op);
        clear.stream_index = 0;
        let mutation = SessionMutation::Record {
            record: LaneRecord::AssistantFrame(clear),
        };
        lazy.apply_mutation(mutation.clone()).unwrap();
        eager.apply_mutation(mutation).unwrap();
        assert_eq!(
            plan.read().await.unwrap(),
            before,
            "an issued read owns its pre-clear generation"
        );
        assert_eq!(
            lazy.record_plan(&query).unwrap().read().await.unwrap(),
            eager.find_records(&query).unwrap()
        );
        assert_eq!(
            lazy.log_plan(&LogOptions::default())
                .unwrap()
                .read()
                .await
                .unwrap(),
            eager.get_log(&LogOptions::default()).unwrap()
        );
        assert!(
            lazy.validate_unused_id("frame-100").is_err(),
            "cleared IDs remain permanently reserved"
        );
        assert!(
            lazy.validate_unused_id("frame-2").is_err(),
            "already retired IDs remain reserved"
        );
    }
}

#[tokio::test]
async fn cold_source_modification_or_short_range_fails_closed() {
    let (lazy, _, reader) = fixture();
    let query = RecordQuery {
        order: Some(EntryOrder::OldestFirst),
        limit: Some(1),
        ..Default::default()
    };
    let offset = lazy
        .cold
        .locations
        .iter()
        .find(|loc| !loc.retired)
        .unwrap()
        .offset as usize;
    reader.bytes.lock().unwrap()[offset] = b' ';
    assert!(lazy
        .record_plan(&query)
        .unwrap()
        .read()
        .await
        .unwrap_err()
        .message
        .contains("source changed"));
    reader.bytes.lock().unwrap().clear();
    assert!(lazy
        .record_plan(&query)
        .unwrap()
        .read()
        .await
        .unwrap_err()
        .message
        .contains("short cold"));
}

#[tokio::test]
async fn published_frame_plan_does_not_block_new_cold_append() {
    let (mut lazy, _, reader) = fixture();
    let query = RecordQuery {
        record_type: Some("assistant_frame"),
        limit: Some(1),
        ..Default::default()
    };
    let old_plan = lazy.record_plan(&query).unwrap();
    let expected = lazy.record_plan(&query).unwrap().read().await.unwrap();
    let frame = frame(101, "main", "new-run", AssistantFrameOp::Append);
    let mutation = SessionMutation::Record {
        record: LaneRecord::AssistantFrame(frame.clone()),
    };
    let line = encode_mutation(&mutation);
    let offset = reader.bytes.lock().unwrap().len();
    reader
        .bytes
        .lock()
        .unwrap()
        .extend_from_slice(line.as_bytes());
    let source: Arc<dyn ReadFile> = reader.clone();
    lazy.defer(&frame, &source, offset as u64, &line).unwrap();
    assert_eq!(old_plan.read().await.unwrap(), expected);
    assert_eq!(
        lazy.record_plan(&query).unwrap().read().await.unwrap(),
        vec![LaneRecord::AssistantFrame(frame)]
    );
    assert_eq!(lazy.sequence(), 101);
}

// Entry hydration regressions: construction follows the streaming loader's
// apply_loaded(mutation, reader, offset, raw) path, not the frame-only defer.
fn entry_batch_reader() -> Arc<MemoryReader> {
    Arc::new(MemoryReader {
        bytes: Mutex::new(Vec::new()),
        reads: AtomicUsize::new(0),
        read_bytes: AtomicUsize::new(0),
    })
}

fn entry_batch_user(seq: u64, text: &str) -> SessionMutation {
    SessionMutation::Entry {
        seq,
        timestamp: 1,
        lane: Some("main".into()),
        entry: Entry::Message(MessageEntry {
            base: EntryBase {
                entry_type: "message".into(),
                id: format!("entry-{seq}"),
                seq,
                parent_id: (seq > 1).then(|| format!("entry-{}", seq - 1)),
                timestamp: 1,
            },
            message: rpi_ai::UserMessage::new(text, 1).into(),
            terminate: None,
        }),
    }
}

fn entry_batch_load(
    state: &mut ResidentState,
    reader: &Arc<MemoryReader>,
    mutation: SessionMutation,
    gap: usize,
) -> (usize, usize) {
    let raw = encode_mutation(&mutation);
    let offset = {
        let mut bytes = reader.bytes.lock().unwrap();
        let offset = bytes.len() + gap;
        bytes.resize(offset, b' ');
        bytes.extend_from_slice(raw.as_bytes());
        offset
    };
    let source: Arc<dyn ReadFile> = reader.clone();
    state
        .apply_loaded(mutation, &source, offset as u64, raw.as_bytes())
        .unwrap();
    (offset, raw.len())
}

fn entry_batch_fixture(count: u64) -> (ResidentState, SessionState, Arc<MemoryReader>) {
    let reader = entry_batch_reader();
    let mut cold = ResidentState::default();
    let mut eager = SessionState::new();
    for seq in 1..=count {
        let mutation = entry_batch_user(seq, &format!("message {seq} 🌊\r\n"));
        eager.apply_mutation(mutation.clone()).unwrap();
        entry_batch_load(&mut cold, &reader, mutation, 0);
    }
    (cold, eager, reader)
}

#[tokio::test]
async fn cold_entry_batch_preserves_selected_order_limit_and_rereads() {
    let (cold, eager, reader) = entry_batch_fixture(60);
    for order in [EntryOrder::OldestFirst, EntryOrder::NewestFirst] {
        let query = EntryQuery {
            order: Some(order),
            limit: Some(50),
            ..Default::default()
        };
        let expected = eager.find_entries(&query).unwrap();
        let selected_bytes: usize = expected
            .iter()
            .map(|entry| {
                encode_mutation(&SessionMutation::Entry {
                    seq: entry.seq(),
                    timestamp: 1,
                    lane: Some("main".into()),
                    entry: entry.clone(),
                })
                .len()
            })
            .sum();
        for _ in 0..2 {
            let reads = reader.reads.load(Ordering::Relaxed);
            let bytes = reader.read_bytes.load(Ordering::Relaxed);
            assert_eq!(
                cold.entry_plan(&query, None).unwrap().read().await.unwrap(),
                expected
            );
            assert_eq!(reader.reads.load(Ordering::Relaxed) - reads, 1);
            assert_eq!(
                reader.read_bytes.load(Ordering::Relaxed) - bytes,
                selected_bytes
            );
        }
    }
    assert!(matches!(
        cold.entry_ref("entry-1"),
        Some(JsonlPayload::Cold { .. })
    ));
    assert!(matches!(
        cold.entry_ref("entry-60"),
        Some(JsonlPayload::Cold { .. })
    ));
}

#[tokio::test]
async fn cold_context_batch_coalesces_fifty_entries_without_warming() {
    let (cold, eager, reader) = entry_batch_fixture(50);
    let bounds = BranchBounds::default();
    let expected = eager.context_on_branch(&bounds, "entry-50").unwrap();
    for reads in 1..=2 {
        let actual = cold
            .context_plan(&bounds, "entry-50")
            .unwrap()
            .read()
            .await
            .unwrap();
        assert_eq!(actual.messages, expected.messages);
        assert_eq!(actual.thinking_level, expected.thinking_level);
        assert_eq!(actual.model, expected.model);
        assert_eq!(actual.active_tool_names, expected.active_tool_names);
        assert_eq!(reader.reads.load(Ordering::Relaxed), reads);
        assert_eq!(
            reader.read_bytes.load(Ordering::Relaxed),
            reader.bytes.lock().unwrap().len() * reads
        );
    }
}

#[tokio::test]
async fn cold_entry_batch_mixes_resident_and_distinct_reader_generations() {
    let mut cold = ResidentState::default();
    let mut eager = SessionState::new();
    let first = entry_batch_reader();
    let second = entry_batch_reader();
    for seq in 1..=5 {
        let mutation = entry_batch_user(seq, &format!("generation payload {seq}"));
        eager.apply_mutation(mutation.clone()).unwrap();
        match seq {
            2 => {
                cold.apply_mutation(mutation).unwrap();
            }
            1 | 3 => {
                entry_batch_load(&mut cold, &first, mutation, 0);
            }
            _ => {
                entry_batch_load(&mut cold, &second, mutation, 0);
            }
        }
    }
    // Both source generations begin at offset zero, but contain different IDs.
    // Keep the plan alive after replacing the entire resident view as well.
    let query = EntryQuery {
        order: Some(EntryOrder::OldestFirst),
        ..Default::default()
    };
    let plan = cold.entry_plan(&query, None).unwrap();
    cold = ResidentState::default();
    assert_eq!(
        plan.read().await.unwrap(),
        eager.find_entries(&query).unwrap()
    );
    assert_eq!(first.reads.load(Ordering::Relaxed), 1);
    assert_eq!(second.reads.load(Ordering::Relaxed), 1);
    for reader in [&first, &second] {
        assert_eq!(
            reader.read_bytes.load(Ordering::Relaxed),
            reader.bytes.lock().unwrap().len()
        );
    }
    assert!(cold.entry_ref("entry-1").is_none());
}

#[tokio::test]
async fn cold_entry_batch_checks_each_selected_line_digest() {
    let (cold, _, reader) = entry_batch_fixture(3);
    let last = match cold.entry_ref("entry-3").unwrap() {
        JsonlPayload::Cold { source, .. } => source.offset as usize,
        _ => panic!("expected cold entry"),
    };
    // Change message text without changing JSON shape, identity, or length.
    // Every row's digest must be checked, even the last row of a merged read.
    {
        let mut bytes = reader.bytes.lock().unwrap();
        let text = bytes[last..]
            .windows(b"message 3".len())
            .position(|window| window == b"message 3")
            .unwrap();
        bytes[last + text] = b'M';
    }
    let error = cold
        .entry_plan(&EntryQuery::default(), None)
        .unwrap()
        .read()
        .await
        .unwrap_err();
    assert!(error.message.contains("source changed"));
    assert_eq!(reader.reads.load(Ordering::Relaxed), 1);
    let error = cold
        .context_plan(&BranchBounds::default(), "entry-3")
        .unwrap()
        .read()
        .await
        .unwrap_err();
    assert!(error.message.contains("source changed"));
}

#[tokio::test]
async fn cold_entry_and_context_batches_reject_short_ranges() {
    let (cold, _, reader) = entry_batch_fixture(3);
    reader.bytes.lock().unwrap().pop();
    let error = cold
        .entry_plan(&EntryQuery::default(), None)
        .unwrap()
        .read()
        .await
        .unwrap_err();
    assert!(error.message.contains("short cold"));
    let error = cold
        .context_plan(&BranchBounds::default(), "entry-3")
        .unwrap()
        .read()
        .await
        .unwrap_err();
    assert!(error.message.contains("short cold"));
    assert_eq!(reader.reads.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn cold_entry_batch_gap_boundary_is_inclusive_and_bounded() {
    for (gap, expected_reads) in [(8192, 1), (8193, 2)] {
        let reader = entry_batch_reader();
        let mut cold = ResidentState::default();
        let (_, first_len) = entry_batch_load(&mut cold, &reader, entry_batch_user(1, "first"), 0);
        let (_, second_len) =
            entry_batch_load(&mut cold, &reader, entry_batch_user(2, "second"), gap);
        let entries = cold
            .entry_plan(&EntryQuery::default(), None)
            .unwrap()
            .read()
            .await
            .unwrap();
        assert_eq!(
            entries.iter().map(Entry::id).collect::<Vec<_>>(),
            vec!["entry-2", "entry-1"]
        );
        assert_eq!(
            reader.reads.load(Ordering::Relaxed),
            expected_reads,
            "gap {gap}"
        );
        assert_eq!(
            reader.read_bytes.load(Ordering::Relaxed),
            first_len + second_len + if expected_reads == 1 { gap } else { 0 }
        );
    }
}

#[tokio::test]
async fn cold_entry_batch_span_boundary_is_inclusive_and_bounded() {
    const MAX_SPAN: usize = 256 * 1024;

    for (span, expected_reads) in [(MAX_SPAN, 1), (MAX_SPAN + 1, 2)] {
        let reader = entry_batch_reader();
        let mut cold = ResidentState::default();
        let (_, first_len) = entry_batch_load(&mut cold, &reader, entry_batch_user(1, "first"), 0);
        let empty_len = encode_mutation(&entry_batch_user(2, "")).len();
        let (_, second_len) = entry_batch_load(
            &mut cold,
            &reader,
            entry_batch_user(2, &"x".repeat(span - first_len - empty_len)),
            0,
        );
        assert_eq!(first_len + second_len, span);
        assert!(second_len < MAX_SPAN);
        let entries = cold
            .entry_plan(&EntryQuery::default(), None)
            .unwrap()
            .read()
            .await
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            reader.reads.load(Ordering::Relaxed),
            expected_reads,
            "span {span}"
        );
        assert_eq!(reader.read_bytes.load(Ordering::Relaxed), span);
    }
}

#[tokio::test]
async fn cold_entry_batch_reads_oversized_single_line_without_neighbors() {
    let reader = entry_batch_reader();
    let mut cold = ResidentState::default();
    let mut eager = SessionState::new();
    for (seq, text) in [
        (1, "before".into()),
        (2, "x".repeat(256 * 1024)),
        (3, "after".into()),
    ] {
        let mutation = entry_batch_user(seq, &text);
        eager.apply_mutation(mutation.clone()).unwrap();
        entry_batch_load(&mut cold, &reader, mutation, 0);
    }
    let query = EntryQuery::default();
    assert_eq!(
        cold.entry_plan(&query, None).unwrap().read().await.unwrap(),
        eager.find_entries(&query).unwrap()
    );
    assert_eq!(reader.reads.load(Ordering::Relaxed), 3);
    assert_eq!(
        reader.read_bytes.load(Ordering::Relaxed),
        reader.bytes.lock().unwrap().len()
    );
}

fn entry_batch_tool(call: &str) -> rpi_agent::message::AgentMessage {
    rpi_ai::ToolResultMessage {
        role: rpi_ai::types::ToolResultRole,
        tool_call_id: call.into(),
        tool_name: "tool".into(),
        content: Vec::new(),
        details: Some(serde_json::Value::Null),
        usage: None,
        added_tool_names: Vec::new(),
        is_error: false,
        timestamp: 1,
    }
    .into()
}

#[tokio::test]
async fn cold_entry_payload_filters_stop_before_corrupt_unconsumed_rows() {
    for outbox in [false, true] {
        let reader = entry_batch_reader();
        let mut cold = ResidentState::default();
        let mut eager = SessionState::new();
        let mut consumed_bytes = 0;
        for seq in 1..=3 {
            let key = if seq == 2 { "wanted" } else { "other" };
            let mut mutation = entry_batch_user(seq, "unused");
            if let SessionMutation::Entry {
                entry: Entry::Message(entry),
                ..
            } = &mut mutation
            {
                entry.message = if outbox {
                    rpi_agent::message::AgentMessage::Custom(
                        rpi_agent::message::CustomMessage::new(
                            "outbox",
                            Vec::new(),
                            json!({"__rpiOutbox":{"key":key}}),
                            1,
                        ),
                    )
                } else {
                    entry_batch_tool(key)
                };
            }
            eager.apply_mutation(mutation.clone()).unwrap();
            let (offset, length) = entry_batch_load(&mut cold, &reader, mutation, 0);
            if seq < 3 {
                consumed_bytes += length;
            } else {
                reader.bytes.lock().unwrap()[offset] = b'!';
            }
        }
        let query = EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            limit: Some(1),
            tool_call_id: (!outbox).then(|| "wanted".into()),
            outbox_key: outbox.then(|| "wanted".into()),
            ..Default::default()
        };
        assert_eq!(
            cold.entry_plan(&query, None).unwrap().read().await.unwrap(),
            eager.find_entries(&query).unwrap()
        );
        assert_eq!(reader.reads.load(Ordering::Relaxed), 2);
        assert_eq!(reader.read_bytes.load(Ordering::Relaxed), consumed_bytes);
        let unlimited = EntryQuery {
            limit: None,
            ..query
        };
        assert!(cold
            .entry_plan(&unlimited, None)
            .unwrap()
            .read()
            .await
            .unwrap_err()
            .message
            .contains("source changed"));
    }
}

#[tokio::test]
async fn cold_entry_batch_restores_rechained_parent_and_live_nulls() {
    let reader = entry_batch_reader();
    let source: Arc<dyn ReadFile> = reader.clone();
    let mut cold = ResidentState::default();
    let mut eager = SessionState::new();
    let first = entry_batch_user(1, "root");
    eager.apply_mutation(first.clone()).unwrap();
    entry_batch_load(&mut cold, &reader, first, 0);
    let mut original = entry_batch_user(2, "unused");
    if let SessionMutation::Entry {
        entry: Entry::Message(entry),
        ..
    } = &mut original
    {
        entry.base.parent_id = None; // Divergent raw row, repaired by the loader.
        entry.message = entry_batch_tool("call");
    }
    let raw = encode_mutation(&original);
    let offset = reader.bytes.lock().unwrap().len();
    reader
        .bytes
        .lock()
        .unwrap()
        .extend_from_slice(raw.as_bytes());
    let mut repaired = original;
    if let SessionMutation::Entry { entry, .. } = &mut repaired {
        entry.base_mut().parent_id = Some("entry-1".into());
    }
    // Loader contract: digest/identity describe the original row, while the
    // resident metadata and presence corrections describe the repaired entry.
    eager.apply_mutation(repaired.clone()).unwrap();
    cold.apply_loaded(repaired, &source, offset as u64, raw.as_bytes())
        .unwrap();
    let query = EntryQuery {
        order: Some(EntryOrder::OldestFirst),
        ..Default::default()
    };
    let entries = cold.entry_plan(&query, None).unwrap().read().await.unwrap();
    assert_eq!(entries, eager.find_entries(&query).unwrap());
    assert_eq!(entries[1].parent_id(), Some("entry-1"));
    match entries[1].as_message().unwrap() {
        rpi_agent::message::AgentMessage::ToolResult(result) => {
            assert_eq!(result.details, Some(serde_json::Value::Null))
        }
        _ => panic!("expected tool result"),
    }
    assert_eq!(reader.reads.load(Ordering::Relaxed), 1);
    let bounds = BranchBounds::default();
    let actual = cold
        .context_plan(&bounds, "entry-2")
        .unwrap()
        .read()
        .await
        .unwrap();
    assert_eq!(
        actual.messages,
        eager
            .context_on_branch(&bounds, "entry-2")
            .unwrap()
            .messages
    );
    assert_eq!(reader.reads.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn cold_entry_batch_releases_retired_resident_before_io() {
    struct ReleaseReader {
        retired: std::sync::Weak<Entry>,
        bytes: Vec<u8>,
    }
    #[async_trait]
    impl ReadFile for ReleaseReader {
        async fn read_all(&self) -> Result<Vec<u8>, FileError> {
            panic!("batch must use range reads")
        }
        async fn read_range(&self, offset: u64, length: usize) -> Result<Vec<u8>, FileError> {
            assert!(
                self.retired.upgrade().is_none(),
                "retired original held across I/O"
            );
            Ok(self.bytes[offset as usize..offset as usize + length].to_vec())
        }
    }
    let SessionMutation::Entry { entry, .. } = entry_batch_user(1, &"x".repeat(1024 * 1024)) else {
        unreachable!()
    };
    let original = Arc::new(entry);
    let mutation = entry_batch_user(2, "cold");
    let bytes = encode_mutation(&mutation).into_bytes();
    let reader: Arc<dyn ReadFile> = Arc::new(ReleaseReader {
        retired: Arc::downgrade(&original),
        bytes: bytes.clone(),
    });
    let source = Arc::new(Location::new(reader, 0, &bytes, &mutation));
    let result = read_entries(vec![
        JsonlPayload::Resident(original),
        JsonlPayload::Cold {
            source,
            parent: Some("entry-1".into()),
            live_nulls: LiveNulls::default(),
        },
    ])
    .await
    .unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[1].parent_id(), Some("entry-1"));
}

#[tokio::test]
async fn cold_entry_batch_empty_and_overflow_are_checked_without_io() {
    assert!(read_entries(Vec::new()).await.unwrap().is_empty());
    let reader = entry_batch_reader();
    let mutation = entry_batch_user(1, "overflow");
    let bytes = encode_mutation(&mutation);
    let source = Arc::new(Location::new(
        reader.clone(),
        u64::MAX,
        bytes.as_bytes(),
        &mutation,
    ));
    let error = read_entries(vec![JsonlPayload::Cold {
        source,
        parent: None,
        live_nulls: LiveNulls::default(),
    }])
    .await
    .unwrap_err();
    assert!(error.to_string().contains("overflow"));
    assert_eq!(reader.reads.load(Ordering::Relaxed), 0);
}

// Transcript paging selects metadata before retaining or reading payloads.
fn transcript_query(limit: usize, before: Option<&str>) -> TranscriptPageQuery {
    TranscriptPageQuery {
        limit,
        before_id: before.map(str::to_owned),
    }
}

fn transcript_ids(page: &TranscriptPage) -> Vec<&str> {
    page.entries.iter().map(Entry::id).collect()
}

fn transcript_fixture() -> (ResidentState, Arc<MemoryReader>) {
    let reader = entry_batch_reader();
    let mut cold = ResidentState::default();
    // Every entry variant appears on main; only messages and compaction belong
    // to the transcript, including tool-result messages after the compaction.
    for seq in 1..=9 {
        let mut mutation = entry_batch_user(seq, &format!("transcript {seq}"));
        if let SessionMutation::Entry { entry, .. } = &mut mutation {
            let mut base = entry.base().clone();
            *entry = match seq {
                2 => {
                    base.entry_type = "model_change".into();
                    Entry::ModelChange(ModelChangeEntry {
                        base,
                        provider: "provider".into(),
                        model_id: "model".into(),
                    })
                }
                3 => {
                    base.entry_type = "compaction".into();
                    Entry::Compaction(CompactionEntry {
                        base,
                        summary: "summary".into(),
                        retained_tail: Vec::new(),
                        tokens_before: 100,
                        details: None,
                        usage: None,
                    })
                }
                4 => {
                    base.entry_type = "thinking_level".into();
                    Entry::ThinkingLevel(ThinkingLevelEntry {
                        base,
                        thinking_level: "high".into(),
                    })
                }
                5 => {
                    base.entry_type = "active_tools".into();
                    Entry::ActiveTools(ActiveToolsEntry {
                        base,
                        active_tool_names: vec!["read".into()],
                    })
                }
                6 => {
                    base.entry_type = "branch_summary".into();
                    Entry::BranchSummary(BranchSummaryEntry {
                        base,
                        from_id: "entry-1".into(),
                        summary: "branch summary".into(),
                        details: None,
                        usage: None,
                    })
                }
                7 => {
                    base.entry_type = "custom".into();
                    Entry::Custom(CustomEntry {
                        base,
                        custom_type: "metadata".into(),
                        data: None,
                    })
                }
                8 => Entry::Message(MessageEntry {
                    base,
                    message: entry_batch_tool("call"),
                    terminate: None,
                }),
                _ => entry.clone(),
            };
        }
        entry_batch_load(&mut cold, &reader, mutation, 0);
    }
    cold.apply_mutation(SessionMutation::Lane {
        seq: 10,
        lane: "side".into(),
        leaf_id: Some("entry-3".into()),
    })
    .unwrap();
    let mut side = entry_batch_user(11, "side transcript");
    if let SessionMutation::Entry { lane, entry, .. } = &mut side {
        *lane = Some("side".into());
        entry.base_mut().parent_id = Some("entry-3".into());
    }
    entry_batch_load(&mut cold, &reader, side, 0);
    // The branch leaf need not itself belong to the transcript projection.
    entry_batch_load(
        &mut cold,
        &reader,
        SessionMutation::Entry {
            seq: 12,
            timestamp: 1,
            lane: Some("side".into()),
            entry: Entry::Custom(CustomEntry {
                base: EntryBase {
                    entry_type: "custom".into(),
                    id: "entry-12".into(),
                    seq: 12,
                    parent_id: Some("entry-11".into()),
                    timestamp: 1,
                },
                custom_type: "metadata".into(),
                data: None,
            }),
        },
        0,
    );
    (cold, reader)
}

#[tokio::test]
async fn cold_transcript_projects_all_kinds_and_counts_only_lane_branch() {
    let (cold, _) = transcript_fixture();
    for (lane, expected, leaf) in [
        (
            "main",
            vec!["entry-1", "entry-3", "entry-8", "entry-9"],
            "entry-9",
        ),
        ("side", vec!["entry-1", "entry-3", "entry-11"], "entry-12"),
    ] {
        let page = cold
            .transcript_plan(lane, &transcript_query(usize::MAX, None))
            .unwrap()
            .read()
            .await
            .unwrap();
        assert_eq!(transcript_ids(&page), expected);
        assert_eq!(page.total, expected.len());
        assert!(!page.has_more);
        assert_eq!(page.leaf_id.as_deref(), Some(leaf));
    }
}

#[tokio::test]
async fn cold_transcript_cursor_must_belong_to_projected_branch_without_io() {
    let (cold, reader) = transcript_fixture();
    for (lane, id) in [
        ("main", "unknown"),
        ("main", "entry-2"),
        ("main", "entry-7"),
        ("main", "entry-11"),
        ("side", "entry-9"),
        ("side", "entry-12"),
    ] {
        let error = cold
            .transcript_plan(lane, &transcript_query(2, Some(id)))
            .err()
            .expect("invalid cursor must fail at planning");
        assert_eq!(error.code, crate::error::SessionErrorCode::InvalidQuery);
        assert_eq!(error.message, format!("beforeId not found: {id}"));
    }
    assert_eq!(reader.reads.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn cold_transcript_exclusive_windows_normalize_zero_and_preserve_total() {
    let (cold, _) = transcript_fixture();
    for (limit, before, ids, has_more) in [
        (0, None, vec!["entry-9"], true),
        (2, None, vec!["entry-8", "entry-9"], true),
        (2, Some("entry-8"), vec!["entry-1", "entry-3"], false),
        (1, Some("entry-9"), vec!["entry-8"], true),
        (usize::MAX, Some("entry-3"), vec!["entry-1"], false),
        (2, Some("entry-1"), vec![], false),
        (
            4,
            None,
            vec!["entry-1", "entry-3", "entry-8", "entry-9"],
            false,
        ),
    ] {
        let page = cold
            .transcript_plan("main", &transcript_query(limit, before))
            .unwrap()
            .read()
            .await
            .unwrap();
        assert_eq!(transcript_ids(&page), ids);
        assert_eq!(page.total, 4);
        assert_eq!(page.has_more, has_more);
        assert_eq!(page.leaf_id.as_deref(), Some("entry-9"));
    }
}

#[tokio::test]
async fn cold_transcript_reads_only_selected_window_and_never_warms() {
    let (cold, _, reader) = entry_batch_fixture(60);
    let expected_bytes: usize = (41..=50)
        .map(|seq| encode_mutation(&entry_batch_user(seq, &format!("message {seq} 🌊\r\n"))).len())
        .sum();
    for attempt in 1..=2 {
        let plan = cold
            .transcript_plan("main", &transcript_query(10, Some("entry-51")))
            .unwrap();
        assert_eq!(plan.selected.len(), 10);
        assert_eq!(reader.reads.load(Ordering::Relaxed), attempt - 1);
        let page = plan.read().await.unwrap();
        let expected: Vec<_> = (41..=50).map(|seq| format!("entry-{seq}")).collect();
        assert_eq!(transcript_ids(&page), expected);
        assert_eq!(page.total, 60);
        assert!(page.has_more);
        assert_eq!(reader.reads.load(Ordering::Relaxed), attempt);
        assert_eq!(
            reader.read_bytes.load(Ordering::Relaxed),
            attempt * expected_bytes
        );
    }
    assert!(matches!(
        cold.entry_ref("entry-41"),
        Some(JsonlPayload::Cold { .. })
    ));
}

#[tokio::test]
async fn cold_transcript_ignores_unselected_corruption_but_rejects_selected_rows() {
    let (cold, reader) = transcript_fixture();
    // Damage a metadata-only row, an off-branch message, and an older message.
    // Unselected rows may be inside a coalesced range but must not be decoded.
    for id in ["entry-7", "entry-11", "entry-1"] {
        let JsonlPayload::Cold { source, .. } = cold.entry_ref(id).unwrap() else {
            panic!("cold fixture");
        };
        reader.bytes.lock().unwrap()[source.offset as usize] = b'!';
    }
    let page = cold
        .transcript_plan("main", &transcript_query(3, None))
        .unwrap()
        .read()
        .await
        .unwrap();
    assert_eq!(transcript_ids(&page), ["entry-3", "entry-8", "entry-9"]);
    assert_eq!(page.total, 4);
    assert_eq!(reader.reads.load(Ordering::Relaxed), 1);
    for (lane, query) in [
        ("main", transcript_query(4, None)),
        ("side", transcript_query(1, None)),
    ] {
        let error = cold
            .transcript_plan(lane, &query)
            .unwrap()
            .read()
            .await
            .unwrap_err();
        assert_eq!(error.code, crate::error::SessionErrorCode::InvalidEntry);
        assert!(error.message.contains("source changed"));
    }
}

#[tokio::test]
async fn cold_transcript_selected_short_read_is_an_error() {
    let (cold, _, reader) = entry_batch_fixture(3);
    reader.bytes.lock().unwrap().pop();
    let error = cold
        .transcript_plan("main", &transcript_query(1, None))
        .unwrap()
        .read()
        .await
        .unwrap_err();
    assert_eq!(error.code, crate::error::SessionErrorCode::Storage);
    assert!(error.message.contains("short cold history read"));
}

#[tokio::test]
async fn cold_transcript_published_plan_keeps_leaf_total_and_reader_after_append_and_replace() {
    let (mut cold, _, old_reader) = entry_batch_fixture(3);
    let plan = cold
        .transcript_plan("main", &transcript_query(2, None))
        .unwrap();
    // Appending does not widen a previously published page or its total.
    entry_batch_load(
        &mut cold,
        &old_reader,
        entry_batch_user(4, "later append"),
        0,
    );
    let current = cold
        .transcript_plan("main", &transcript_query(1, None))
        .unwrap()
        .read()
        .await
        .unwrap();
    assert_eq!(transcript_ids(&current), ["entry-4"]);
    assert_eq!(current.total, 4);
    assert_eq!(current.leaf_id.as_deref(), Some("entry-4"));
    // Simulate atomic state replacement using a distinct reader generation.
    let (replacement, _, new_reader) = entry_batch_fixture(1);
    cold = replacement;
    assert!(cold.entry_ref("entry-3").is_none());
    drop(old_reader);
    let page = plan.read().await.unwrap();
    assert_eq!(transcript_ids(&page), ["entry-2", "entry-3"]);
    assert_eq!(page.total, 3);
    assert!(page.has_more);
    assert_eq!(page.leaf_id.as_deref(), Some("entry-3"));
    assert_eq!(new_reader.reads.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn cold_transcript_empty_lanes_and_first_cursor_do_not_read_payloads() {
    let mut cold = ResidentState::default();
    cold.apply_mutation(SessionMutation::Lane {
        seq: 1,
        lane: "side".into(),
        leaf_id: None,
    })
    .unwrap();
    for lane in ["main", "side"] {
        let page = cold
            .transcript_plan(lane, &transcript_query(0, None))
            .unwrap()
            .read()
            .await
            .unwrap();
        assert!(page.entries.is_empty());
        assert_eq!(page.total, 0);
        assert!(!page.has_more);
        assert_eq!(page.leaf_id, None);
        let error = cold
            .transcript_plan(lane, &transcript_query(1, Some("missing")))
            .err()
            .unwrap();
        assert_eq!(error.code, crate::error::SessionErrorCode::InvalidQuery);
        assert_eq!(error.message, "beforeId not found: missing");
    }
    let error = cold
        .transcript_plan("missing", &transcript_query(1, None))
        .err()
        .unwrap();
    assert_eq!(error.code, crate::error::SessionErrorCode::InvalidLane);
    let (cold, reader) = transcript_fixture();
    let page = cold
        .transcript_plan("main", &transcript_query(2, Some("entry-1")))
        .unwrap()
        .read()
        .await
        .unwrap();
    assert!(page.entries.is_empty());
    assert_eq!(page.total, 4);
    assert!(!page.has_more);
    assert_eq!(reader.reads.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn cold_transcript_metadata_only_lane_and_resident_payload_defensive_copies() {
    let mut cold = ResidentState::default();
    cold.apply_mutation(SessionMutation::Entry {
        seq: 1,
        timestamp: 1,
        lane: Some("main".into()),
        entry: Entry::Custom(CustomEntry {
            base: EntryBase {
                entry_type: "custom".into(),
                id: "entry-1".into(),
                seq: 1,
                parent_id: None,
                timestamp: 1,
            },
            custom_type: "metadata".into(),
            data: None,
        }),
    })
    .unwrap();
    let page = cold
        .transcript_plan("main", &transcript_query(1, None))
        .unwrap()
        .read()
        .await
        .unwrap();
    assert!(page.entries.is_empty());
    assert_eq!(page.total, 0);
    assert!(!page.has_more);
    assert_eq!(page.leaf_id.as_deref(), Some("entry-1"));
    let error = cold
        .transcript_plan("main", &transcript_query(1, Some("entry-1")))
        .err()
        .unwrap();
    assert_eq!(error.code, crate::error::SessionErrorCode::InvalidQuery);
    cold.apply_mutation(entry_batch_user(2, "resident message"))
        .unwrap();
    let mut page = cold
        .transcript_plan("main", &transcript_query(usize::MAX, None))
        .unwrap()
        .read()
        .await
        .unwrap();
    assert_eq!(transcript_ids(&page), ["entry-2"]);
    assert_eq!(page.total, 1);
    page.entries[0].base_mut().id = "mutated copy".into();
    let original = cold
        .transcript_plan("main", &transcript_query(1, None))
        .unwrap()
        .read()
        .await
        .unwrap();
    assert_eq!(transcript_ids(&original), ["entry-2"]);
}

#[tokio::test]
async fn detached_replay_skips_changed_retired_bodies_but_validates_live_frames() {
    let (lazy, eager, reader) = fixture();
    let retired_offset = lazy
        .cold
        .locations
        .iter()
        .find(|loc| loc.retired)
        .unwrap()
        .offset as usize;
    reader.bytes.lock().unwrap()[retired_offset] = b' ';
    let detached = lazy.into_detached().await.unwrap();
    assert_eq!(
        detached.get_log(&LogOptions::default()).unwrap(),
        eager.get_log(&LogOptions::default()).unwrap()
    );
    assert!(detached.validate_unused_id("frame-2").is_err());
    let (lazy, _, reader) = fixture();
    let live_offset = lazy
        .cold
        .locations
        .iter()
        .find(|loc| !loc.retired)
        .unwrap()
        .offset as usize;
    reader.bytes.lock().unwrap()[live_offset] = b' ';
    assert!(lazy.into_detached().await.is_err());
}

#[tokio::test]
async fn cold_clear_skips_resident_record_scan_without_hot_appends() {
    for count in [8, 16, 32] {
        let reader = entry_batch_reader();
        let source: Arc<dyn ReadFile> = reader.clone();
        let mut commits = ResidentState::default();
        for index in 0..count {
            let seq = commits.sequence() + 1;
            let commit = frame(
                seq,
                "main",
                &format!("commit-{index}"),
                AssistantFrameOp::CommitStream,
            );
            commits
                .apply_mutation(SessionMutation::Record {
                    record: LaneRecord::AssistantFrame(commit),
                })
                .unwrap();
            let seq = commits.sequence() + 1;
            let clear = frame(
                seq,
                "main",
                &format!("clear-{index}"),
                AssistantFrameOp::ClearRun,
            );
            let line = encode_mutation(&SessionMutation::Record {
                record: LaneRecord::AssistantFrame(clear),
            });
            let offset = reader.bytes.lock().unwrap().len() as u64;
            reader
                .bytes
                .lock()
                .unwrap()
                .extend_from_slice(line.as_bytes());
            commits
                .apply_loaded_frame(
                    super::super::cold_decode::parse_cold_frame(&line).unwrap(),
                    &source,
                    offset,
                    line.as_bytes(),
                )
                .unwrap();
        }
        assert_eq!(
            commits.retire_record_checks(),
            0,
            "commit/clear count {count}"
        );

        let mut operations = ResidentState::default();
        for index in 0..count {
            let seq = operations.sequence() + 1;
            operations
                .apply_mutation(SessionMutation::Record {
                    record: LaneRecord::OperationStarted(OperationStartedRecord {
                        base: RecordBase {
                            id: format!("operation-{index}"),
                            seq,
                            timestamp: 1,
                            lane: "main".into(),
                        },
                        source_leaf_id: None,
                        intent: OperationIntent::Navigation {
                            target_id: None,
                            summarize: false,
                            custom_instructions: None,
                            label: None,
                            summary_entry_id: None,
                        },
                    }),
                })
                .unwrap();
            let seq = operations.sequence() + 1;
            let clear = frame(
                seq,
                "main",
                &format!("operation-clear-{index}"),
                AssistantFrameOp::ClearRun,
            );
            let line = encode_mutation(&SessionMutation::Record {
                record: LaneRecord::AssistantFrame(clear),
            });
            let offset = reader.bytes.lock().unwrap().len() as u64;
            reader
                .bytes
                .lock()
                .unwrap()
                .extend_from_slice(line.as_bytes());
            operations
                .apply_loaded_frame(
                    super::super::cold_decode::parse_cold_frame(&line).unwrap(),
                    &source,
                    offset,
                    line.as_bytes(),
                )
                .unwrap();
        }
        assert_eq!(
            operations.retire_record_checks(),
            0,
            "operation/clear count {count}"
        );
    }
}

#[test]
fn fast_clear_still_retires_a_resident_append() {
    let reader = entry_batch_reader();
    let source: Arc<dyn ReadFile> = reader.clone();
    let mut state = ResidentState::default();
    let mut append = frame(1, "main", "resident", AssistantFrameOp::Append);
    append.stream_index = 0;
    state
        .apply_mutation(SessionMutation::Record {
            record: LaneRecord::AssistantFrame(append),
        })
        .unwrap();
    let mut clear = frame(2, "main", "resident", AssistantFrameOp::ClearStream);
    clear.stream_index = 0;
    let line = encode_mutation(&SessionMutation::Record {
        record: LaneRecord::AssistantFrame(clear),
    });
    state
        .apply_loaded_frame(
            super::super::cold_decode::parse_cold_frame(&line).unwrap(),
            &source,
            0,
            line.as_bytes(),
        )
        .unwrap();
    assert!(state.records().is_empty());
    assert!(state.retire_record_checks() > 0);
}

#[tokio::test]
async fn fast_cold_retirement_reindexes_new_appends_and_preserves_cross_lane_snapshots() {
    let mut fast = ResidentState::default();
    let mut full = ResidentState::default();
    let mut eager = SessionState::new();
    let reader = entry_batch_reader();
    let source: Arc<dyn ReadFile> = reader.clone();
    let lane = SessionMutation::Lane {
        seq: 1,
        lane: "side".into(),
        leaf_id: None,
    };
    fast.apply_mutation(lane.clone()).unwrap();
    full.apply_mutation(lane.clone()).unwrap();
    eager.apply_mutation(lane).unwrap();
    let mut snapshots = Vec::new();
    // Clears deliberately originate on a different lane from some appends:
    // historical matching is by run/stream, not by lane.
    for (index, (lane, run, stream, op)) in [
        ("main", "a", 0, AssistantFrameOp::Append),
        ("side", "a", 0, AssistantFrameOp::Append),
        ("main", "a", 1, AssistantFrameOp::Append),
        ("side", "b", 0, AssistantFrameOp::Append),
        ("main", "b", 1, AssistantFrameOp::Append),
        ("side", "a", 0, AssistantFrameOp::ClearStream),
        ("main", "a", 0, AssistantFrameOp::ClearStream),
        ("side", "a", 0, AssistantFrameOp::Append),
        ("main", "a", 1, AssistantFrameOp::ClearRun),
        ("side", "a", 1, AssistantFrameOp::ClearRun),
        ("side", "a", 0, AssistantFrameOp::Append),
        ("main", "a", 1, AssistantFrameOp::Append),
        ("main", "b", 0, AssistantFrameOp::ClearStream),
        ("main", "a", 0, AssistantFrameOp::Append),
        ("side", "a", 1, AssistantFrameOp::ClearStream),
        ("main", "missing", 0, AssistantFrameOp::ClearRun),
    ]
    .into_iter()
    .enumerate()
    {
        let seq = index as u64 + 2;
        let mut record = frame(seq, lane, run, op);
        record.stream_index = stream;
        let mutation = SessionMutation::Record {
            record: LaneRecord::AssistantFrame(record),
        };
        let line = encode_mutation(&mutation);
        let offset = reader.bytes.lock().unwrap().len() as u64;
        reader
            .bytes
            .lock()
            .unwrap()
            .extend_from_slice(line.as_bytes());
        let metadata = super::super::cold_decode::parse_cold_frame(&line)
            .expect("all fixture rows must exercise the metadata-only path");
        fast.apply_loaded_frame(metadata, &source, offset, line.as_bytes())
            .unwrap();
        full.apply_loaded(mutation.clone(), &source, offset, line.as_bytes())
            .unwrap();
        eager.apply_mutation(mutation).unwrap();
        assert_eq!(fast.sequence(), eager.sequence());
        for lane in [None, Some("main".into()), Some("side".into())] {
            let query = RecordQuery {
                lane,
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            };
            let expected = eager.find_records(&query).unwrap();
            assert_eq!(
                fast.record_plan(&query).unwrap().read().await.unwrap(),
                expected
            );
            assert_eq!(
                full.record_plan(&query).unwrap().read().await.unwrap(),
                expected
            );
        }
        if [6, 9, 15].contains(&seq) {
            let query = RecordQuery::default();
            snapshots.push((
                fast.record_plan(&query).unwrap(),
                eager.find_records(&query).unwrap(),
            ));
        }
    }
    assert_eq!(
        fast.cold
            .locations
            .iter()
            .filter(|location| location.append && !location.retired)
            .map(|location| location.seq)
            .collect::<Vec<_>>(),
        [6, 12, 15],
        "unrelated streams survive; appends after repeated clears are indexed again"
    );
    for (plan, expected) in snapshots {
        assert_eq!(
            plan.read().await.unwrap(),
            expected,
            "retirement must detach old Location Arcs"
        );
    }
    assert_eq!(
        fast.log_plan(&LogOptions::default())
            .unwrap()
            .read()
            .await
            .unwrap(),
        eager.get_log(&LogOptions::default()).unwrap()
    );
    assert!(fast.validate_unused_id("frame-2").is_err());
    let mut reused = frame(18, "main", "new-run", AssistantFrameOp::Append);
    reused.base.id = "frame-2".into();
    let mutation = SessionMutation::Record {
        record: LaneRecord::AssistantFrame(reused),
    };
    let line = encode_mutation(&mutation);
    let metadata = super::super::cold_decode::parse_cold_frame(&line).unwrap();
    assert!(fast
        .apply_loaded_frame(metadata, &source, 0, line.as_bytes())
        .is_err());
    assert!(full
        .apply_loaded(mutation.clone(), &source, 0, line.as_bytes())
        .is_err());
    assert!(eager.apply_mutation(mutation).is_err());
}
