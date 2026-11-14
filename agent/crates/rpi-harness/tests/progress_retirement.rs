//! Terminal history is the only authority for retrying failed payload retirement.
use rpi_harness::agent_harness::AgentHarness;
use rpi_harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
use rpi_harness::session::types::*;
use rpi_harness::session::{
    DefaultIdGenerator, InMemorySessionStorage, Session, SqliteSessionRepo, SystemClock,
};
use rpi_harness::types::AgentHarnessOptions;
use rpi_tools::{FileSystem, OsExecutionEnv};
use std::sync::Arc;

fn base(id: &str) -> RecordBase {
    RecordBase {
        id: id.into(),
        seq: 0,
        lane: "main".into(),
        timestamp: 0,
    }
}
fn payload(id: &str, run: &str) -> AssistantFrameRecord {
    AssistantFrameRecord {
        base: base(id),
        run_id: run.into(),
        stream_index: 0,
        op: AssistantFrameOp::Append,
        frame: Some(serde_json::json!({"payload":"reclaimable data".repeat(100)})),
    }
}
fn started(run: &str) -> LaneRecord {
    LaneRecord::OperationStarted(OperationStartedRecord {
        base: base(run),
        source_leaf_id: None,
        intent: OperationIntent::Run {
            original_prompt: vec![],
            initial_messages: vec![],
            system_prompt_override: None,
            resume_data: None,
        },
    })
}
fn finished(run: &str, outcome: OperationOutcome) -> LaneRecord {
    LaneRecord::OperationFinished(OperationFinishedRecord {
        base: base(&format!("finish-{run}")),
        run_id: run.into(),
        outcome,
        error: None,
        result: None,
    })
}
fn options(id: &str) -> SessionCreateOptions {
    SessionCreateOptions {
        id: Some(id.into()),
        parent_session_id: None,
        metadata: None,
    }
}
async fn native(fs: Arc<dyn FileSystem>, path: &str, id: &str) -> JsonlSessionStorage {
    JsonlSessionStorage::create(
        fs,
        path,
        JsonlV4Header::new(id.into(), 1, "/".into(), None, None, None),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn terminal_proof_is_lane_scoped_and_cleanup_preserves_both_sequence_domains() {
    let tmp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(tmp.path().into()));
    let repo = SqliteSessionRepo::open(tmp.path().join("test.db")).unwrap();
    let memory = InMemorySessionStorage::new(
        SessionMetadata {
            id: "memory".into(),
            created_at: 1,
            parent_session_id: None,
        },
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    );
    let stores: Vec<Arc<dyn SessionStorage>> = vec![
        Arc::new(memory),
        Arc::new(native(fs, "session.jsonl", "jsonl").await),
        Arc::new(repo.create(&options("sql")).await.unwrap()),
    ];
    for store in stores {
        store.create_lane("other", None).await.unwrap();
        for (run, outcome) in [
            ("done", OperationOutcome::Completed),
            ("aborted", OperationOutcome::Aborted),
            ("failed", OperationOutcome::Failed),
        ] {
            store.append_record(started(run)).await.unwrap();
            store.append_record(finished(run, outcome)).await.unwrap();
        }
        // Open/suspended operations have no terminal record; unknown IDs are
        // also not evidence of completion and must remain recoverable.
        store.append_record(started("suspended")).await.unwrap();
        let mut other = payload("f-other", "done");
        other.base.lane = "other".into();
        let frames = store
            .append_progress(vec![
                payload("f-done", "done"),
                payload("f-done-2", "done"),
                payload("f-aborted", "aborted"),
                payload("f-failed", "failed"),
                payload("f-suspended", "suspended"),
                payload("f-unknown", "unknown"),
                other,
            ])
            .await
            .unwrap();
        let last_sequence = frames.last().unwrap().base.seq;
        let history =
            serde_json::to_value(store.get_log(&LogOptions::default()).await.unwrap()).unwrap();
        let stats = serde_json::to_value(store.get_stats().await.unwrap()).unwrap();
        let query = RecordPageQuery {
            lane: "main".into(),
            after_seq: None,
            before_seq: None,
            limit: 100,
            from_end: false,
        };
        let page = store.record_page(&query).await.unwrap();
        assert_eq!(store.prune_completed_progress().await.unwrap(), 4);
        assert_eq!(store.prune_completed_progress().await.unwrap(), 0);
        assert_eq!(
            store.read_progress("done").await.unwrap()[0].base.lane,
            "other"
        );
        assert!(store.read_progress("aborted").await.unwrap().is_empty());
        assert!(store.read_progress("failed").await.unwrap().is_empty());
        assert_eq!(store.read_progress("suspended").await.unwrap().len(), 1);
        assert_eq!(store.read_progress("unknown").await.unwrap().len(), 1);
        assert_eq!(
            serde_json::to_value(store.get_log(&LogOptions::default()).await.unwrap()).unwrap(),
            history
        );
        assert_eq!(
            serde_json::to_value(store.get_stats().await.unwrap()).unwrap(),
            stats
        );
        let after_page = store.record_page(&query).await.unwrap();
        assert_eq!(after_page.total, page.total);
        assert_eq!(
            serde_json::to_value(after_page.records).unwrap(),
            serde_json::to_value(page.records).unwrap()
        );
        let next = store
            .append_progress(vec![payload("f-done", "next")])
            .await
            .unwrap();
        assert_eq!(
            next[0].base.seq,
            last_sequence + 1,
            "cleanup must not consume sequence or retain retired IDs"
        );
    }
}

#[tokio::test]
async fn startup_and_session_switch_retry_retirement_without_touching_history() {
    let tmp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(tmp.path().into()));
    for path in ["startup.jsonl", "switch.jsonl"] {
        let storage = native(fs.clone(), path, path).await;
        storage.append_record(started("done")).await.unwrap();
        storage
            .append_progress(vec![
                payload("garbage", "done"),
                payload("survivor", "unknown"),
            ])
            .await
            .unwrap();
        // Simulate a refused clear followed by successful normal settlement.
        storage
            .append_record(finished("done", OperationOutcome::Completed))
            .await
            .unwrap();
        let history = fs.read_binary_file(path, None).await.unwrap();
        let before = fs
            .read_binary_file(&format!("{path}.progress.jsonl"), None)
            .await
            .unwrap()
            .len();
        drop(storage);
        let reopened = JsonlSessionStorage::load(
            fs.clone(),
            path,
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let session = Session::new(Arc::new(reopened), None);
        let harness = if path == "startup.jsonl" {
            AgentHarness::create(AgentHarnessOptions {
                session,
                allow_existing_session: true,
                ..Default::default()
            })
            .await
            .unwrap()
        } else {
            let harness = AgentHarness::create(Default::default()).await.unwrap();
            harness.set_session(session).await.unwrap();
            harness
        };
        assert!(harness
            .session()
            .read_progress("done")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            harness
                .session()
                .read_progress("unknown")
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(fs.read_binary_file(path, None).await.unwrap(), history);
        assert!(
            fs.read_binary_file(&format!("{path}.progress.jsonl"), None)
                .await
                .unwrap()
                .len()
                < before
        );
        drop(harness);
        let reopened = JsonlSessionStorage::load(
            fs.clone(),
            path,
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let sidecar = fs
            .read_binary_file(&format!("{path}.progress.jsonl"), None)
            .await
            .unwrap();
        assert_eq!(reopened.prune_completed_progress().await.unwrap(), 0);
        assert_eq!(
            fs.read_binary_file(&format!("{path}.progress.jsonl"), None)
                .await
                .unwrap(),
            sidecar
        );
    }
}

#[tokio::test]
async fn sqlite_cleanup_reads_current_terminal_proof_without_rewriting_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("test.db");
    let first = SqliteSessionRepo::open(&path).unwrap();
    let stale = first.create(&options("session")).await.unwrap();
    stale.append_record(started("done")).await.unwrap();
    stale
        .append_progress(vec![
            payload("garbage", "done"),
            payload("survivor", "unknown"),
        ])
        .await
        .unwrap();
    let second = SqliteSessionRepo::open(&path).unwrap();
    let current = second.open(&stale.metadata()).await.unwrap();
    let foreign = second.create(&options("foreign")).await.unwrap();
    foreign
        .append_record(finished("unknown", OperationOutcome::Completed))
        .await
        .unwrap();
    foreign
        .append_progress(vec![payload("foreign-frame", "done")])
        .await
        .unwrap();
    current
        .append_record(finished("done", OperationOutcome::Completed))
        .await
        .unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    let snapshot: String = db
        .query_row(
            "SELECT state_json FROM sessions WHERE id='session'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stale.prune_completed_progress().await.unwrap(),
        1,
        "must not trust stale Memory mirror"
    );
    assert!(current.read_progress("done").await.unwrap().is_empty());
    assert_eq!(current.read_progress("unknown").await.unwrap().len(), 1);
    let after: String = db
        .query_row(
            "SELECT state_json FROM sessions WHERE id='session'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(snapshot, after);
    assert_eq!(
        foreign.read_progress("done").await.unwrap().len(),
        1,
        "cleanup must not affect another session's matching run ID"
    );
}
