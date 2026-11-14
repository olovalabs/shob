//! Recovery payloads have independent storage, cursors and retirement.
use rpi_harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
use rpi_harness::session::types::*;
use rpi_harness::session::{
    DefaultIdGenerator, InMemorySessionStorage, SqliteSessionRepo, SystemClock,
};
use rpi_tools::{FileContent, FileSystem, OsExecutionEnv};
use std::sync::Arc;

#[tokio::test]
async fn jsonl_requires_its_companion_and_forks_only_committed_history() {
    use rpi_harness::session::jsonl::{JsonlSessionRepo, JsonlSessionRepoOptions};
    let tmp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(tmp.path().into()));
    let storage = JsonlSessionStorage::create(
        fs.clone(),
        "parent.jsonl",
        JsonlV4Header::new("parent".into(), 1, "/".into(), None, None, None),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    storage
        .append_progress(vec![frame("recover", "main", "run", 0)])
        .await
        .unwrap();
    let child = storage
        .fork(
            "child.jsonl",
            JsonlV4Header::new(
                "child".into(),
                2,
                "/".into(),
                Some("parent".into()),
                None,
                None,
            ),
            &ForkOptions::default(),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
    assert!(child.read_progress("run").await.unwrap().is_empty());
    assert_eq!(storage.read_progress("run").await.unwrap().len(), 1);
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: fs.clone(),
        sessions_root: ".".into(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });
    fs.write_file(
        "parent.jsonl.progress.jsonl.tmp",
        FileContent::Text("unpublished".into()),
        None,
    )
    .await
    .unwrap();
    repo.delete_by_jsonl_metadata(&storage.jsonl_metadata())
        .await
        .unwrap();
    for path in [
        "parent.jsonl",
        "parent.jsonl.progress.jsonl",
        "parent.jsonl.progress.jsonl.tmp",
    ] {
        assert!(!fs.exists(path, None).await.unwrap());
    }
    fs.remove("child.jsonl.progress.jsonl", false, false, None)
        .await
        .unwrap();
    assert!(
        JsonlSessionStorage::load(
            fs,
            "child.jsonl",
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new())
        )
        .await
        .is_err(),
        "missing recovery companion must not become an empty successful recovery"
    );
}

#[tokio::test]
async fn sidecar_recovery_preserves_the_bound_entry_and_retires_payloads_before_reopening_twice() {
    use rpi_ai::frames::AssistantMessageFrame;
    use rpi_ai::types::{StopReason, TextContent, TextContentType};
    use rpi_harness::agent_harness::AgentHarness;
    use rpi_harness::session::Session;
    use rpi_harness::types::AgentHarnessOptions;
    let tmp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(tmp.path().into()));
    let storage = JsonlSessionStorage::create(
        fs.clone(),
        "session.jsonl",
        JsonlV4Header::new("recover".into(), 1, "/".into(), None, None, None),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let session = Session::new(Arc::new(storage), None);
    session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: RecordBase {
                id: "run".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 0,
            },
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        }))
        .await
        .unwrap();
    let partial = rpi_ai::providers::faux::faux_assistant_message("", StopReason::Pending);
    let values = [
        AssistantMessageFrame::Start { partial },
        AssistantMessageFrame::TextStart {
            content_index: 0,
            content: TextContent {
                kind: TextContentType,
                text: String::new(),
                text_signature: None,
            },
        },
        AssistantMessageFrame::TextDelta {
            content_index: 0,
            delta: "sidecar-only durable prefix".into(),
        },
    ];
    session
        .append_progress(
            values
                .into_iter()
                .enumerate()
                .map(|(index, value)| AssistantFrameRecord {
                    frame: Some(serde_json::to_value(value).unwrap()),
                    ..frame(&format!("f{index}"), "main", "run", 0)
                })
                .collect(),
        )
        .await
        .unwrap();
    session
        .append_record(LaneRecord::AssistantFrame(AssistantFrameRecord {
            op: AssistantFrameOp::CommitStream,
            frame: Some(serde_json::json!({"entryId":"reserved-assistant"})),
            ..frame("bind", "main", "run", 0)
        }))
        .await
        .unwrap();
    assert!(!fs
        .read_text_file("session.jsonl", None)
        .await
        .unwrap()
        .contains("sidecar-only durable prefix"));
    drop(session);
    for _ in 0..2 {
        let storage = JsonlSessionStorage::load(
            fs.clone(),
            "session.jsonl",
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let session = Session::new(Arc::new(storage), None);
        let harness = AgentHarness::create(AgentHarnessOptions {
            session,
            allow_existing_session: true,
            ..Default::default()
        })
        .await
        .unwrap();
        let entries = harness
            .session()
            .find_entries(&EntryQuery {
                entry_type: Some("message"),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id(), "reserved-assistant");
        let Entry::Message(message) = &entries[0] else {
            panic!("recovered message")
        };
        let rpi_agent::AgentMessage::Assistant(assistant) = &message.message else {
            panic!("recovered assistant")
        };
        assert_eq!(assistant.stop_reason, StopReason::Error);
        assert!(serde_json::to_string(assistant)
            .unwrap()
            .contains("sidecar-only durable prefix"));
        assert!(harness
            .session()
            .read_progress("run")
            .await
            .unwrap()
            .is_empty());
        assert!(!fs
            .read_text_file("session.jsonl.progress.jsonl", None)
            .await
            .unwrap()
            .contains("sidecar-only durable prefix"));
    }
}

fn frame(id: &str, lane: &str, run: &str, stream: usize) -> AssistantFrameRecord {
    AssistantFrameRecord {
        base: RecordBase {
            id: id.into(),
            seq: 0,
            lane: lane.into(),
            timestamp: 0,
        },
        run_id: run.into(),
        stream_index: stream,
        op: AssistantFrameOp::Append,
        frame: Some(
            serde_json::json!({"type":"text_delta","contentIndex":0,"delta":"payload".repeat(160)}),
        ),
    }
}
fn clear(id: &str, lane: &str, run: &str, stream: Option<usize>) -> AssistantFrameRecord {
    AssistantFrameRecord {
        op: if stream.is_some() {
            AssistantFrameOp::ClearStream
        } else {
            AssistantFrameOp::ClearRun
        },
        stream_index: stream.unwrap_or(0),
        frame: None,
        ..frame(id, lane, run, 0)
    }
}
fn abort(id: &str) -> LaneRecord {
    LaneRecord::AbortRequested(AbortRequestedRecord {
        base: RecordBase {
            id: id.into(),
            seq: 0,
            lane: "main".into(),
            timestamp: 0,
        },
        run_id: "audit".into(),
    })
}

#[tokio::test]
async fn payloads_do_not_advance_history_and_retirement_is_lane_run_and_stream_scoped() {
    let tmp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(tmp.path().into()));
    let sql = SqliteSessionRepo::open(tmp.path().join("sessions.db")).unwrap();
    let sqlite = sql
        .create(&SessionCreateOptions {
            id: Some("sqlite".into()),
            parent_session_id: None,
            metadata: None,
        })
        .await
        .unwrap();
    let jsonl = JsonlSessionStorage::create(
        fs,
        "session.jsonl",
        JsonlV4Header::new("jsonl".into(), 1, "/".into(), None, None, None),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let memory = InMemorySessionStorage::new(
        SessionMetadata {
            id: "memory".into(),
            created_at: 1,
            parent_session_id: None,
        },
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    );
    for storage in [
        Arc::new(memory) as Arc<dyn SessionStorage>,
        Arc::new(jsonl),
        Arc::new(sqlite),
    ] {
        storage.create_lane("side", None).await.unwrap();
        let before = storage.append_record(abort("before")).await.unwrap().seq();
        storage
            .append_progress(vec![
                frame("a", "main", "one", 0),
                frame("b", "main", "one", 1),
                frame("c", "side", "one", 0),
                frame("d", "main", "two", 0),
            ])
            .await
            .unwrap();
        assert_eq!(storage.read_progress("one").await.unwrap().len(), 3);
        assert_eq!(
            storage
                .record_page(&RecordPageQuery {
                    lane: "main".into(),
                    after_seq: None,
                    before_seq: None,
                    limit: 10,
                    from_end: false
                })
                .await
                .unwrap()
                .total,
            1
        );
        assert_eq!(
            storage.append_record(abort("after")).await.unwrap().seq(),
            before + 1
        );
        assert!(storage
            .append_progress(vec![
                frame("new", "main", "one", 0),
                frame("bad", "missing", "one", 0)
            ])
            .await
            .is_err());
        assert_eq!(
            storage.read_progress("one").await.unwrap().len(),
            3,
            "whole batch must be validated first"
        );
        assert!(storage
            .append_progress(vec![frame("a", "main", "one", 0)])
            .await
            .is_err());
        assert!(
            storage
                .append_progress(vec![clear("a", "main", "one", None)])
                .await
                .is_err(),
            "retirement IDs follow the same duplicate rule as append IDs"
        );
        storage
            .append_progress(vec![clear("clear-stream", "main", "one", Some(0))])
            .await
            .unwrap();
        assert_eq!(
            storage
                .read_progress("one")
                .await
                .unwrap()
                .iter()
                .map(|frame| frame.base.id.as_str())
                .collect::<Vec<_>>(),
            ["b", "c"]
        );
        storage
            .append_progress(vec![clear("clear-run", "main", "one", None)])
            .await
            .unwrap();
        assert_eq!(storage.read_progress("one").await.unwrap()[0].base.id, "c");
        assert_eq!(storage.read_progress("two").await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn jsonl_checkpoint_physically_retires_payloads_and_reopens_the_new_append_inode() {
    let tmp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(tmp.path().into()));
    let storage = JsonlSessionStorage::create(
        fs.clone(),
        "session.jsonl",
        JsonlV4Header::new("native".into(), 1, "/".into(), None, None, None),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let history = fs.read_binary_file("session.jsonl", None).await.unwrap();
    for batch in 0..8 {
        storage
            .append_progress(
                (0..32)
                    .map(|index| frame(&format!("f-{batch}-{index}"), "main", "run", 0))
                    .collect(),
            )
            .await
            .unwrap();
    }
    storage
        .append_progress(vec![frame("keep", "main", "other-run", 0)])
        .await
        .unwrap();
    let before = fs
        .read_binary_file("session.jsonl.progress.jsonl", None)
        .await
        .unwrap();
    storage
        .append_progress(vec![clear("retire", "main", "run", None)])
        .await
        .unwrap();
    let after = fs
        .read_binary_file("session.jsonl.progress.jsonl", None)
        .await
        .unwrap();
    eprintln!(
        "progress bytes: before={} after={} history={}",
        before.len(),
        after.len(),
        history.len()
    );
    assert!(after.len() < before.len() / 10);
    assert!(!String::from_utf8_lossy(&after).contains("f-0-0"));
    assert!(storage.read_progress("run").await.unwrap().is_empty());
    assert_eq!(
        fs.read_binary_file("session.jsonl", None).await.unwrap(),
        history
    );
    storage
        .append_progress(vec![frame("new", "main", "next", 0)])
        .await
        .unwrap();
    drop(storage);
    for _ in 0..2 {
        let storage = JsonlSessionStorage::load(
            fs.clone(),
            "session.jsonl",
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        assert!(storage.read_progress("run").await.unwrap().is_empty());
        assert_eq!(
            storage.read_progress("other-run").await.unwrap()[0].base.id,
            "keep"
        );
        assert_eq!(
            storage.read_progress("next").await.unwrap()[0].base.id,
            "new"
        );
    }
    // A crash staging a checkpoint must not promote an unpublished tmp file.
    fs.write_file(
        "session.jsonl.progress.jsonl.tmp",
        FileContent::Text("incomplete".into()),
        None,
    )
    .await
    .unwrap();
    let storage = JsonlSessionStorage::load(
        fs.clone(),
        "session.jsonl",
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    assert_eq!(storage.read_progress("next").await.unwrap().len(), 1);
}

#[tokio::test]
async fn sqlite_progress_survives_connections_and_is_not_forked() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("sessions.db");
    let repo = SqliteSessionRepo::open(&path).unwrap();
    let options = SessionCreateOptions {
        id: Some("original".into()),
        parent_session_id: None,
        metadata: None,
    };
    let storage = repo.create(&options).await.unwrap();
    storage
        .append_progress(vec![frame("f", "main", "run", 0)])
        .await
        .unwrap();
    let other = SqliteSessionRepo::open(&path).unwrap();
    let reopened = other.open(&storage.metadata()).await.unwrap();
    assert_eq!(reopened.read_progress("run").await.unwrap().len(), 1);
    let fork = repo
        .fork(
            &storage.metadata(),
            &SessionCreateOptions {
                id: Some("fork".into()),
                ..options
            },
            &ForkOptions::default(),
        )
        .await
        .unwrap();
    assert!(fork.read_progress("run").await.unwrap().is_empty());
    reopened
        .append_progress(vec![clear("clear", "main", "run", None)])
        .await
        .unwrap();
    assert!(storage.read_progress("run").await.unwrap().is_empty());
    repo.delete(&storage.metadata()).await.unwrap();
    assert!(storage
        .append_progress(vec![frame("late", "main", "run", 0)])
        .await
        .is_err());
}
