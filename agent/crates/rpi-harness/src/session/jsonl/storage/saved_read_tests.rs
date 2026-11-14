//! Saved-only budgets must not disappear when stable readers select streaming.
use super::*;
use std::time::{Duration, Instant};

const PATH: &str = "/history.jsonl";
const PROGRESS: &str = "/history.jsonl.progress.jsonl";
fn future() -> Instant {
    Instant::now() + Duration::from_secs(60)
}
fn query(after_seq: Option<u64>, deadline: Instant) -> SavedSessionReadQuery {
    SavedSessionReadQuery {
        session_id: header().id,
        limit: 1,
        after_seq,
        custom_types: Vec::new(),
        deadline,
    }
}
async fn saved(
    fs: Arc<MemoryFs>,
    query: SavedSessionReadQuery,
) -> SessionResult<SavedSessionReadData> {
    JsonlSessionStorage::read_saved_session(
        fs,
        PATH,
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
        query,
    )
    .await
}
async fn assert_unchanged(fs: &MemoryFs, history: &str, progress: &str) {
    assert_eq!(fs.read_text_file(PATH, None).await.unwrap(), history);
    assert_eq!(fs.read_text_file(PROGRESS, None).await.unwrap(), progress);
    assert_eq!(fs.list_dir("/", None).await.unwrap().len(), 2);
}

#[tokio::test]
async fn expired_saved_deadline_fails_before_any_history_io_in_both_paths() {
    for mode in [Mode::Stable, Mode::ReadAllOnly] {
        // No file exists: MemoryFs would panic if replay tried to open it.
        let fs = MemoryFs::new(mode);
        let error = saved(fs, query(None, Instant::now() - Duration::from_secs(1)))
            .await
            .err()
            .unwrap();
        assert!(error.message.contains("deadline"));
    }
}

#[tokio::test]
async fn saved_strict_streaming_eager_parity_preserves_both_sources() {
    for mode in [Mode::Stable, Mode::ReadAllOnly, Mode::LegacyRanged] {
        for damage in [
            "none",
            "duplicate",
            "torn",
            "unterminated",
            "schema",
            "bad-frame",
            "torn-progress",
            "unterminated-progress",
        ] {
            let fs = MemoryFs::new(mode);
            let storage = create(fs.clone()).await;
            storage
                .append_entry(
                    ProvisionedEntry {
                        id: "message".into(),
                        kind: ProvisionedKind::Message {
                            message: rpi_ai::UserMessage::new("visible", 1).into(),
                            terminate: None,
                        },
                    },
                    "main",
                )
                .await
                .unwrap();
            let mut history = fs.read_text_file(PATH, None).await.unwrap();
            let mut progress = fs.read_text_file(PROGRESS, None).await.unwrap();
            match damage {
                "duplicate" => history.push_str(&format!("{}\n", history.lines().last().unwrap())),
                "torn" => history.push_str("{unacknowledged"),
                "unterminated" => {
                    history.pop();
                }
                "schema" => history.push_str("{}\n"),
                "bad-frame" => history.push_str(
                    "{\"kind\":\"record\",\"type\":\"assistant_frame\",\"op\":\"append\"}\n",
                ),
                "torn-progress" => progress.push_str("{unacknowledged"),
                "unterminated-progress" => {
                    progress.pop();
                }
                _ => {}
            }
            fs.write_file(PATH, FileContent::Text(history.clone()), None)
                .await
                .unwrap();
            fs.write_file(PROGRESS, FileContent::Text(progress.clone()), None)
                .await
                .unwrap();
            let history_mtime = fs.file_info(PATH, None).await.unwrap().mtime_ms;
            let progress_mtime = fs.file_info(PROGRESS, None).await.unwrap().mtime_ms;
            let result = saved(fs.clone(), query(None, future())).await;
            assert_eq!(result.is_ok(), damage == "none", "{damage}");
            if let Ok(data) = result {
                assert!(data.cursor_valid);
                assert_eq!(data.messages.len(), 1);
                assert!(!data.has_more);
                assert!(!data.native_pending);
                assert!(!data.progress_pending);
                assert!(data.custom_entries.is_empty());
            }
            assert_unchanged(&fs, &history, &progress).await;
            assert_eq!(
                fs.file_info(PATH, None).await.unwrap().mtime_ms,
                history_mtime
            );
            assert_eq!(
                fs.file_info(PROGRESS, None).await.unwrap().mtime_ms,
                progress_mtime
            );
        }
    }
}

#[tokio::test]
async fn saved_history_mutation_limit_applies_to_streaming_and_eager() {
    for mode in [Mode::Stable, Mode::ReadAllOnly] {
        let fs = MemoryFs::new(mode);
        create(fs.clone()).await;
        let mut history = fs.read_text_file(PATH, None).await.unwrap();
        for seq in 1..=ReadPolicy::SAVED_MUTATION_LIMIT + 1 {
            history.push_str(&encode_mutation(&SessionMutation::FactName {
                seq: seq as u64,
                name: None,
            }));
        }
        fs.write_file(PATH, FileContent::Text(history.clone()), None)
            .await
            .unwrap();
        let progress = fs.read_text_file(PROGRESS, None).await.unwrap();
        let error = saved(fs.clone(), query(None, future()))
            .await
            .err()
            .unwrap();
        assert!(error.message.contains("history mutation limit"), "{error}");
        assert_unchanged(&fs, &history, &progress).await;
    }
}

fn progress_frame(seq: u64, op: AssistantFrameOp) -> AssistantFrameRecord {
    AssistantFrameRecord {
        base: RecordBase {
            id: format!("f{seq}"),
            seq,
            lane: "main".into(),
            timestamp: 1,
        },
        run_id: "run".into(),
        stream_index: 0,
        op,
        frame: (op == AssistantFrameOp::Append).then(|| serde_json::json!({"delta":"bounded"})),
    }
}
fn checkpoint(frames: Vec<AssistantFrameRecord>) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "format": "assistant-progress-v1", "session_id": header().id,
            "state": { "sequence": frames.len(), "frames": frames },
        })
    )
}
async fn inspect(
    fs: Arc<MemoryFs>,
    deadline: Instant,
    records: usize,
    frames: usize,
) -> SessionResult<bool> {
    let store = super::super::super::progress::JsonlProgressStore::new(
        fs,
        PATH,
        &header().id,
        Arc::new(FakeClock::new()),
    );
    store
        .inspect_pending_without_repair_with_deadline(deadline, records, frames)
        .await
}

#[tokio::test]
async fn saved_progress_bounds_records_checkpoint_and_active_frames_without_repair() {
    for case in ["checkpoint", "records", "active", "clear", "deadline"] {
        let fs = MemoryFs::new(Mode::Stable);
        create(fs.clone()).await;
        let mut progress = checkpoint(if case == "checkpoint" {
            (1..=3)
                .map(|seq| progress_frame(seq, AssistantFrameOp::Append))
                .collect()
        } else {
            Vec::new()
        });
        if case == "records" || case == "active" || case == "clear" {
            for seq in 1..=3 {
                let op = if case == "records" || (case == "clear" && seq == 3) {
                    AssistantFrameOp::ClearRun
                } else {
                    AssistantFrameOp::Append
                };
                progress.push_str(&serde_json::to_string(&progress_frame(seq, op)).unwrap());
                progress.push('\n');
            }
        }
        fs.write_file(PROGRESS, FileContent::Text(progress.clone()), None)
            .await
            .unwrap();
        let history = fs.read_text_file(PATH, None).await.unwrap();
        let deadline = if case == "deadline" {
            Instant::now() - Duration::from_secs(1)
        } else {
            future()
        };
        let result = inspect(
            fs.clone(),
            deadline,
            if case == "records" { 2 } else { 3 },
            2,
        )
        .await;
        if case == "clear" {
            assert!(!result.unwrap());
        } else {
            let error = result.unwrap_err();
            let expected = match case {
                "records" => "record limit",
                "deadline" => "deadline",
                _ => "frame limit",
            };
            assert!(error.message.contains(expected), "{case}: {error}");
        }
        assert_unchanged(&fs, &history, &progress).await;
        // Normal restore/transition inspection has no saved-only caps.
        if case != "deadline" {
            assert!(inspect(fs, future(), 10, 10).await.is_ok());
        }
    }
}

#[tokio::test]
async fn saved_cursor_validity_is_data_not_an_early_branch_error() {
    for mode in [Mode::Stable, Mode::ReadAllOnly] {
        let fs = MemoryFs::new(mode);
        let storage = create(fs.clone()).await;
        let entry = storage
            .append_entry(large_message("e1"), "main")
            .await
            .unwrap();
        for (cursor, valid) in [
            (None, true),
            (Some(0), false),
            (Some(entry.seq()), true),
            (Some(99), false),
        ] {
            let data = saved(fs.clone(), query(cursor, future())).await.unwrap();
            assert_eq!(data.cursor_valid, valid);
            assert_eq!(data.leaf_id.as_deref(), Some("e1"));
            if cursor == Some(entry.seq()) {
                assert!(data.messages.is_empty());
            }
        }
    }
}

#[tokio::test]
async fn saved_cold_replay_retains_source_integrity_checks() {
    for mode in [Mode::Stable, Mode::LegacyRanged] {
        let fs = MemoryFs::new(mode);
        let original = create(fs.clone()).await;
        original.append_record(frame()).await.unwrap();
        let storage = JsonlSessionStorage::load_impl(
            fs.clone(),
            PATH,
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
            ReadPolicy::SavedReadCold(future()),
        )
        .await
        .unwrap();
        assert!(storage
            .state
            .lock()
            .await
            .find_records(&RecordQuery::default())
            .unwrap()
            .is_empty());
        let reader = fs.reader(PATH);
        let mut bytes = reader.bytes.lock().unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        *bytes = text
            .replace("legacy payload", "forged payload")
            .into_bytes();
        drop(bytes);
        // Metadata inspection never hydrates unrelated historical payloads.
        storage.facts().await.unwrap();
        assert!(storage.find_records(&RecordQuery::default()).await.is_err());
    }
}
