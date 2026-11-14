//! Synthetic selection-cost probe; never opens a user's session.
//! Run explicitly in release mode with --ignored --nocapture.
use rpi_agent::AgentMessage;
use rpi_ai::types::{UserContent, UserMessage};
use rpi_harness::session::{
    jsonl::{encode_header, encode_mutation, JsonlSessionStorage, JsonlV4Header},
    types::*,
    DefaultIdGenerator, SystemClock,
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use std::{collections::HashSet, sync::Arc, time::Instant};

fn base(id: String, seq: u64) -> RecordBase {
    RecordBase {
        id,
        seq,
        timestamp: 1,
        lane: "main".into(),
    }
}
fn enqueue(id: &str, seq: u64, text: &str) -> LaneRecord {
    let full = provisioned_into_entry(
        ProvisionedEntry {
            id: id.into(),
            kind: ProvisionedKind::Message {
                message: AgentMessage::User(UserMessage::new(UserContent::Text(text.into()), 0)),
                terminate: None,
            },
        },
        0,
        None,
        0,
    );
    let mut target = full.to_flat_json();
    for key in ["seq", "timestamp", "parentId"] {
        target.as_object_mut().unwrap().remove(key);
    }
    LaneRecord::QueueEnqueued(QueueEnqueuedRecord {
        base: base(format!("enqueue-{id}"), seq),
        queue: QueueKind::Steer,
        run_id: None,
        target,
    })
}
fn query(kind: &'static str) -> RecordQuery {
    RecordQuery {
        lane: Some("main".into()),
        record_type: Some(kind),
        order: Some(EntryOrder::OldestFirst),
        ..Default::default()
    }
}
async fn previous_selection(store: &dyn SessionStorage) -> Vec<LaneRecord> {
    let mut records = store.find_records(&query("queue_enqueued")).await.unwrap();
    records.extend(store.find_records(&query("queue_cancelled")).await.unwrap());
    records.sort_unstable_by_key(LaneRecord::seq);
    let mut cancelled = HashSet::new();
    let mut enqueued = Vec::new();
    for record in records {
        match record {
            LaneRecord::QueueCancelled(cancel) => {
                cancelled.insert(cancel.entry_id);
            }
            LaneRecord::QueueEnqueued(_) => enqueued.push(record),
            _ => {}
        }
    }
    let mut pending = Vec::new();
    for record in enqueued {
        if let LaneRecord::QueueEnqueued(enqueued) = &record {
            let id = enqueued.target["id"].as_str().unwrap();
            if !cancelled.contains(id) && store.get_entry(id).await.unwrap().is_none() {
                pending.push(record);
            }
        }
    }
    pending
}
async fn current_selection(store: &dyn SessionStorage) -> Vec<LaneRecord> {
    store
        .find_records(&RecordQuery {
            pending_queue_only: true,
            ..query("queue_enqueued")
        })
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "synthetic release-mode cost probe, not a timing gate"]
async fn compare_queue_selection_cost() {
    const RETIRED: usize = 4096;
    const FRAMES: usize = 4096;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("synthetic.jsonl");
    let header = JsonlV4Header::new("synthetic".into(), 1, "/".into(), None, None, None);
    let mut raw = encode_header(&header);
    let mut seq = 0;
    let payload = "retired queue payload ".repeat(64);
    for index in 0..RETIRED {
        let id = format!("retired-{index}");
        seq += 1;
        raw.push_str(&encode_mutation(&SessionMutation::Record {
            record: enqueue(&id, seq, &payload),
        }));
        seq += 1;
        raw.push_str(&encode_mutation(&SessionMutation::Record {
            record: LaneRecord::QueueCancelled(QueueCancelledRecord {
                base: base(format!("cancel-{index}"), seq),
                entry_id: id,
                run_id: None,
            }),
        }));
    }
    for id in ["pending-first", "pending-second"] {
        seq += 1;
        raw.push_str(&encode_mutation(&SessionMutation::Record {
            record: enqueue(id, seq, id),
        }));
    }
    for index in 0..FRAMES {
        seq += 1;
        raw.push_str(&encode_mutation(&SessionMutation::Record {
            record: LaneRecord::AssistantFrame(AssistantFrameRecord {
                base: base(format!("frame-{index}"), seq),
                run_id: "old".into(),
                stream_index: 0,
                op: AssistantFrameOp::Append,
                frame: Some(serde_json::json!({"delta":"unrelated frame ".repeat(64)})),
            }),
        }));
    }
    std::fs::write(&path, &raw).unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let started = Instant::now();
    let store = JsonlSessionStorage::load(
        fs,
        path.to_str().unwrap(),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let load_ns = started.elapsed().as_nanos();
    let expected = current_selection(&store).await;
    assert_eq!(expected.len(), 2);
    assert_eq!(previous_selection(&store).await, expected);
    let old_enqueues = store.find_records(&query("queue_enqueued")).await.unwrap();
    let old_cancels = store.find_records(&query("queue_cancelled")).await.unwrap();
    let old_bytes = serde_json::to_vec(&old_enqueues).unwrap().len()
        + serde_json::to_vec(&old_cancels).unwrap().len();
    let selected_bytes = serde_json::to_vec(&expected).unwrap().len();
    let old_rows = old_enqueues.len() + old_cancels.len();
    drop((old_enqueues, old_cancels));
    let mut old_ns = Vec::new();
    let mut selected_ns = Vec::new();
    // Alternate ordering to reduce a fixed warm-cache/order bias.
    for round in 0..9 {
        for previous in if round % 2 == 0 {
            [true, false]
        } else {
            [false, true]
        } {
            let start = Instant::now();
            let result = if previous {
                previous_selection(&store).await
            } else {
                current_selection(&store).await
            };
            let elapsed = start.elapsed().as_nanos();
            assert_eq!(result, expected);
            if previous {
                old_ns.push(elapsed);
            } else {
                selected_ns.push(elapsed);
            }
        }
    }
    old_ns.sort_unstable();
    selected_ns.sort_unstable();
    assert_eq!(old_rows, RETIRED * 2 + 2);
    assert!(selected_bytes < old_bytes / 100);
    assert_eq!(std::fs::read(&path).unwrap(), raw.as_bytes());
    eprintln!(
        "queue_selection_probe {}",
        serde_json::json!({
            "fixture_bytes":raw.len(), "retired":RETIRED, "cold_frames":FRAMES, "load_ns":load_ns,
            "old_rows":old_rows, "selected_rows":expected.len(), "old_json_bytes":old_bytes,
            "selected_json_bytes":selected_bytes, "old_median_ns":old_ns[4], "selected_median_ns":selected_ns[4],
            "samples":9, "release":!cfg!(debug_assertions)
        })
    );
}
