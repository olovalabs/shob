//! Real-file cold frame loading must preserve the eager storage contract.
use rpi_harness::session::jsonl::{
    encode_header, encode_mutation, JsonlSessionStorage, JsonlV4Header,
};
use rpi_harness::session::types::*;
use rpi_harness::session::{DefaultIdGenerator, SystemClock};
use rpi_tools::{FileContent, FileSystem, InMemoryExecutionEnv, OsExecutionEnv};
use serde_json::json;
use std::sync::Arc;

fn record(seq: u64, op: AssistantFrameOp) -> SessionMutation {
    SessionMutation::Record {
        record: LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: RecordBase {
                id: format!("f-{seq}"),
                seq,
                timestamp: 1,
                lane: "main".into(),
            },
            run_id: "pending-run".into(),
            stream_index: 0,
            op,
            frame: Some(json!({"type":"text_delta","delta":"中🌊\r\n","entryId":"reserved-entry"})),
        }),
    }
}
fn text() -> String {
    let header = JsonlV4Header::new("cold-test".into(), 1, "/".into(), None, None, None);
    let mut text = encode_header(&header);
    for seq in 1..=8 {
        let mutation = if seq == 6 {
            SessionMutation::FactName {
                seq,
                name: Some("label".into()),
            }
        } else {
            record(
                seq,
                if seq == 7 {
                    AssistantFrameOp::CommitStream
                } else {
                    AssistantFrameOp::Append
                },
            )
        };
        text.push_str(&encode_mutation(&mutation));
    }
    text
}
async fn load(fs: Arc<dyn FileSystem>, path: &str) -> JsonlSessionStorage {
    JsonlSessionStorage::load(
        fs,
        path,
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn read_snapshot_preserves_cold_frames_without_repair_or_file_capabilities() {
    for damage in ["none", "duplicate", "torn", "unterminated"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let original = text();
        let mut raw = original.clone();
        match damage {
            "duplicate" => raw.push_str(&encode_mutation(&record(2, AssistantFrameOp::Append))),
            "torn" => raw.push_str("{\"seq\":9,"),
            "unterminated" => {
                raw.pop();
            }
            _ => {}
        }
        std::fs::write(&path, &raw).unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let snapshot = JsonlSessionStorage::read_snapshot(
            fs.clone(),
            path.to_str().unwrap(),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        assert_eq!(
            Arc::strong_count(&fs),
            1,
            "snapshot must release file/storage capabilities"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        let normal = load(fs, path.to_str().unwrap()).await;
        let expected = normal.find_records(&RecordQuery::default()).await.unwrap();
        let expected_log = normal.get_log(&LogOptions::default()).await.unwrap();
        assert_eq!(
            snapshot
                .find_records(&RecordQuery::default())
                .await
                .unwrap(),
            expected
        );
        assert_eq!(
            snapshot.get_log(&LogOptions::default()).await.unwrap(),
            expected_log
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        snapshot.set_name(Some("memory only")).await.unwrap();
        let detached_log = snapshot.get_log(&LogOptions::default()).await.unwrap();
        assert_eq!(detached_log.len(), expected_log.len() + 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        drop(normal);
        // Neither in-place changes nor deletion can alter a detached snapshot.
        std::fs::write(&path, "replaced").unwrap();
        assert_eq!(
            snapshot
                .find_records(&RecordQuery::default())
                .await
                .unwrap(),
            expected
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            snapshot.get_log(&LogOptions::default()).await.unwrap(),
            detached_log
        );
    }
}

async fn assert_equal(left: &JsonlSessionStorage, right: &JsonlSessionStorage) {
    for order in [EntryOrder::OldestFirst, EntryOrder::NewestFirst] {
        for limit in [None, Some(1), Some(3)] {
            let query = RecordQuery {
                order: Some(order),
                limit,
                ..Default::default()
            };
            assert_eq!(
                left.find_records(&query).await.unwrap(),
                right.find_records(&query).await.unwrap()
            );
        }
    }
    assert_eq!(
        left.get_log(&LogOptions::default()).await.unwrap(),
        right.get_log(&LogOptions::default()).await.unwrap()
    );
    for from_end in [false, true] {
        let query = RecordPageQuery {
            lane: "main".into(),
            after_seq: Some(1),
            before_seq: Some(8),
            limit: 3,
            from_end,
        };
        let l = left.record_page(&query).await.unwrap();
        let r = right.record_page(&query).await.unwrap();
        assert_eq!(l.total, r.total);
        assert_eq!(l.records, r.records);
    }
}

#[tokio::test]
async fn real_cold_and_unsupported_eager_readers_agree_across_header_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let raw = text().replace('\n', "\r\n");
    std::fs::write(&path, &raw).unwrap();
    let os: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let mem: Arc<dyn FileSystem> = Arc::new(InMemoryExecutionEnv::with_cwd("/".into()));
    mem.write_file("/copy.jsonl", FileContent::Text(raw.clone()), None)
        .await
        .unwrap();
    let lazy = load(os.clone(), path.to_str().unwrap()).await;
    let eager = load(mem, "/copy.jsonl").await;
    assert_equal(&lazy, &eager).await;
    assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
    let upgraded = lazy.enable_progress_under_exclusive_lock().await.unwrap();
    assert_equal(&upgraded, &eager).await;
    assert!(
        upgraded
            .append_record(match record(1, AssistantFrameOp::Append) {
                SessionMutation::Record { record } => record,
                _ => unreachable!(),
            })
            .await
            .is_err(),
        "cold IDs remain reserved"
    );
    let appended = upgraded
        .append_record(LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: RecordBase {
                id: "new-frame".into(),
                seq: 0,
                timestamp: 0,
                lane: "main".into(),
            },
            run_id: "new-run".into(),
            stream_index: 0,
            op: AssistantFrameOp::ClearRun,
            frame: None,
        }))
        .await
        .unwrap();
    assert_eq!(appended.seq(), 9);
    let reopened = load(os, path.to_str().unwrap()).await;
    assert_equal(&upgraded, &reopened).await;
}

#[tokio::test]
async fn cold_generation_survives_native_path_replacement_but_rejects_in_place_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.jsonl");
    let raw = text();
    std::fs::write(&path, &raw).unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let loaded = load(fs.clone(), path.to_str().unwrap()).await;
    let expected = loaded.find_records(&RecordQuery::default()).await.unwrap();
    let previous = dir.path().join("previous.jsonl");
    std::fs::rename(&path, &previous).unwrap();
    std::fs::write(&path, raw.replace("reserved-entry", "replaced-entry")).unwrap();
    assert_eq!(
        loaded.find_records(&RecordQuery::default()).await.unwrap(),
        expected
    );
    std::fs::write(&previous, raw.replace("reserved-entry", "modified-entry")).unwrap();
    assert!(loaded.find_records(&RecordQuery::default()).await.is_err());
}

#[tokio::test]
async fn cold_indexes_survive_duplicate_torn_and_unterminated_tail_repairs() {
    for repair in ["duplicate", "torn", "unterminated", "gap"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let mut raw = text();
        match repair {
            "duplicate" => raw.push_str(&encode_mutation(&record(2, AssistantFrameOp::Append))),
            "torn" => raw.push_str("{\"seq\":9,"),
            "unterminated" => {
                raw.pop();
            }
            "gap" => raw.push_str(&encode_mutation(&record(11, AssistantFrameOp::ClearRun))),
            _ => unreachable!(),
        }
        std::fs::write(&path, &raw).unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let original = load(fs.clone(), path.to_str().unwrap()).await;
        let reloaded = load(fs.clone(), path.to_str().unwrap()).await;
        assert_equal(&original, &reloaded).await;
        let appended = original
            .append_record(LaneRecord::AssistantFrame(AssistantFrameRecord {
                base: RecordBase {
                    id: "appended".into(),
                    seq: 0,
                    timestamp: 0,
                    lane: "main".into(),
                },
                run_id: "run".into(),
                stream_index: 0,
                op: AssistantFrameOp::ClearStream,
                frame: None,
            }))
            .await
            .unwrap();
        assert_eq!(appended.seq(), if repair == "gap" { 12 } else { 9 });
        assert_equal(&original, &load(fs, path.to_str().unwrap()).await).await;
    }
}

#[tokio::test]
async fn read_snapshot_preserves_historical_payloads_after_source_replacement() {
    for damage in ["none", "duplicate", "torn", "unterminated"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let original = text();
        let mut raw = original.clone();
        match damage {
            "duplicate" => raw.push_str(&encode_mutation(&record(2, AssistantFrameOp::Append))),
            "torn" => raw.push_str("{\"seq\":9,"),
            "unterminated" => {
                raw.pop();
            }
            _ => {}
        }
        std::fs::write(&path, &raw).unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let snapshot = JsonlSessionStorage::read_snapshot(
            fs.clone(),
            path.to_str().unwrap(),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        let normal = load(fs, path.to_str().unwrap()).await;
        assert_eq!(
            snapshot
                .find_records(&RecordQuery::default())
                .await
                .unwrap(),
            normal.find_records(&RecordQuery::default()).await.unwrap()
        );
        assert_eq!(
            snapshot.get_log(&LogOptions::default()).await.unwrap(),
            normal.get_log(&LogOptions::default()).await.unwrap()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        snapshot.set_name(Some("memory only")).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        // Neither later source changes nor dropped file readers can alter the
        // already-returned snapshot's historical payloads.
        std::fs::write(&path, "replaced").unwrap();
        assert_eq!(
            snapshot
                .find_records(&RecordQuery::default())
                .await
                .unwrap()
                .len(),
            7
        );
    }
}

#[tokio::test]
async fn divergent_duplicate_cold_payload_is_rejected_without_source_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.jsonl");
    let mut raw = text();
    raw.push_str(
        &encode_mutation(&record(2, AssistantFrameOp::Append))
            .replace("reserved-entry", "diverged-entry"),
    );
    std::fs::write(&path, &raw).unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let result = JsonlSessionStorage::load(
        fs,
        path.to_str().unwrap(),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
}

#[tokio::test]
async fn nonframe_recovery_report_and_checkpoint_do_not_read_all_cold_rows() {
    use rpi_harness::{runtime::SessionRuntime, session::Session};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.jsonl");
    let raw = text();
    std::fs::write(&path, &raw).unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let store = load(fs, path.to_str().unwrap()).await;
    // If a report accidentally fetches all history, the first cold row's
    // digest check will fail. The newest row for the checkpoint is unchanged.
    std::fs::write(&path, raw.replacen("reserved-entry", "corruptd-entry", 1)).unwrap();
    let runtime = SessionRuntime::new(Session::new(Arc::new(store), None));
    assert!(runtime.recover_lane("main").await.unwrap().is_clean());
    assert_eq!(runtime.checkpoint("main").await.unwrap().last_record_seq, 8);
}
