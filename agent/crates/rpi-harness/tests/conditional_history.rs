//! Storage-level conditional acceptance, including a queued outbox whose leaf
//! does not advance. Neither progress nor an unconditional CAS retry may bypass it.
use rpi_harness::agent_harness::{AgentHarness, BranchWriteGuard, ScopedEntry};
use rpi_harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
use rpi_harness::session::memory::Clock;
use rpi_harness::session::types::*;
use rpi_harness::session::{
    DefaultIdGenerator, InMemorySessionStorage, Session, SqliteSessionRepo, SystemClock,
};
use rpi_harness::types::AgentHarnessOptions;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Barrier,
};

fn base(id: &str) -> RecordBase {
    RecordBase {
        id: id.into(),
        seq: 0,
        lane: "main".into(),
        timestamp: 0,
    }
}
fn started() -> LaneRecord {
    LaneRecord::OperationStarted(OperationStartedRecord {
        base: base("open-run"),
        source_leaf_id: None,
        intent: OperationIntent::Run {
            original_prompt: vec![],
            initial_messages: vec![],
            system_prompt_override: None,
            resume_data: None,
        },
    })
}
fn state(id: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.into(),
        kind: ProvisionedKind::Custom {
            custom_type: "goal-state".into(),
            data: None,
        },
    }
}

#[tokio::test]
async fn conditional_history_revision_is_atomic_and_ignores_progress_on_all_backends() {
    let temp = tempfile::tempdir().unwrap();
    let clock = Arc::new(SystemClock);
    let ids = Arc::new(DefaultIdGenerator::new());
    let memory = InMemorySessionStorage::new(
        SessionMetadata {
            id: "memory".into(),
            created_at: 0,
            parent_session_id: None,
        },
        clock.clone(),
        ids.clone(),
    );
    let repo = SqliteSessionRepo::open(temp.path().join("sessions.db")).unwrap();
    let sqlite = repo
        .create(&SessionCreateOptions {
            id: Some("sqlite".into()),
            parent_session_id: None,
            metadata: None,
        })
        .await
        .unwrap();
    let path = temp
        .path()
        .join("session.jsonl")
        .to_string_lossy()
        .replace('\\', "/");
    let jsonl = JsonlSessionStorage::create(
        Arc::new(rpi_tools::OsExecutionEnv::with_cwd(temp.path().into())),
        &path,
        JsonlV4Header::new(
            "jsonl".into(),
            1,
            temp.path().display().to_string(),
            None,
            None,
            None,
        ),
        clock,
        ids,
    )
    .await
    .unwrap();
    let stores: Vec<Arc<dyn SessionStorage>> =
        vec![Arc::new(memory), Arc::new(sqlite), Arc::new(jsonl)];
    for storage in stores {
        let initial = storage.lane_branch("main").await.unwrap();
        storage
            .append_entry(state("branch-root"), "main")
            .await
            .unwrap();
        let appended = storage.lane_branch("main").await.unwrap();
        assert_eq!(initial.generation, appended.generation);
        assert_eq!(appended.leaf_id.as_deref(), Some("branch-root"));
        storage.move_lane("main", None).await.unwrap();
        let away = storage.lane_branch("main").await.unwrap();
        storage
            .move_lane("main", Some("branch-root"))
            .await
            .unwrap();
        let returned = storage.lane_branch("main").await.unwrap();
        assert_eq!(returned.leaf_id, appended.leaf_id);
        assert!(away.generation > appended.generation);
        assert!(returned.generation > away.generation);
        storage
            .create_lane("branch-side", Some("branch-root"))
            .await
            .unwrap();
        assert!(storage.lane_branch("branch-side").await.unwrap().generation > returned.generation);
        assert!(storage.lane_branch("missing").await.is_err());
        let original = storage.history_revision().await.unwrap();
        storage
            .append_progress(vec![AssistantFrameRecord {
                base: base("frame"),
                run_id: "progress-run".into(),
                stream_index: 0,
                op: AssistantFrameOp::Append,
                frame: Some(serde_json::json!({"payload":"transient"})),
            }])
            .await
            .unwrap();
        assert_eq!(storage.history_revision().await.unwrap(), original);
        storage
            .append_entry_if_revision(state("accepted"), "main", original)
            .await
            .unwrap();
        assert!(storage
            .append_entry_if_revision(state("rejected"), "main", original)
            .await
            .is_err());
        assert!(storage.get_entry("rejected").await.unwrap().is_none());
        assert!(storage
            .append_record_if_revision(started(), original)
            .await
            .is_err());
        assert!(storage
            .find_open_operations("main", None)
            .await
            .unwrap()
            .is_empty());
        let fresh = storage.history_revision().await.unwrap();
        storage
            .append_record_if_revision(started(), fresh)
            .await
            .unwrap();
        assert_eq!(storage.history_revision().await.unwrap(), fresh + 1);
        assert_eq!(
            storage.read_progress("progress-run").await.unwrap().len(),
            1
        );
    }
    let jsonl = JsonlSessionStorage::load(
        Arc::new(rpi_tools::OsExecutionEnv::with_cwd(temp.path().into())),
        &path,
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let reopened = jsonl.lane_branch("main").await.unwrap();
    assert_eq!(reopened.generation, 3); // root append, move-away, move-back
    assert_eq!(reopened.leaf_id.as_deref(), Some("accepted"));
    let reopened_sqlite = repo
        .open(&SessionMetadata {
            id: "sqlite".into(),
            created_at: 0,
            parent_session_id: None,
        })
        .await
        .unwrap();
    assert_eq!(reopened_sqlite.lane_branch("main").await.unwrap(), reopened);
}

struct RendezvousClock {
    armed: AtomicBool,
    calls: AtomicUsize,
    barrier: Barrier,
}
impl Clock for RendezvousClock {
    fn now_ms(&self) -> i64 {
        if self.armed.load(Ordering::SeqCst) && self.calls.fetch_add(1, Ordering::SeqCst) < 2 {
            self.barrier.wait();
        }
        1
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_queued_notices_compare_revision_even_when_the_leaf_does_not_change() {
    let temp = tempfile::tempdir().unwrap();
    let clock = Arc::new(RendezvousClock {
        armed: AtomicBool::new(false),
        calls: AtomicUsize::new(0),
        barrier: Barrier::new(2),
    });
    let path = temp.path().join("outbox.db");
    let first_repo = SqliteSessionRepo::with_clock_ids(
        &path,
        clock.clone(),
        Arc::new(DefaultIdGenerator::new()),
    )
    .unwrap();
    let first = first_repo
        .create(&SessionCreateOptions {
            id: Some("outbox".into()),
            parent_session_id: None,
            metadata: None,
        })
        .await
        .unwrap();
    let metadata = first.metadata();
    first.append_entry(state("source"), "main").await.unwrap();
    let second_repo = SqliteSessionRepo::with_clock_ids(
        &path,
        clock.clone(),
        Arc::new(DefaultIdGenerator::new()),
    )
    .unwrap();
    let second = second_repo.open(&metadata).await.unwrap();
    let a = AgentHarness::create(AgentHarnessOptions {
        session: Session::new(Arc::new(first), None),
        ..Default::default()
    })
    .await
    .unwrap();
    let b = AgentHarness::create(AgentHarnessOptions {
        session: Session::new(Arc::new(second), None),
        ..Default::default()
    })
    .await
    .unwrap();
    a.session().append_record(started()).await.unwrap();
    let guard = BranchWriteGuard {
        session_id: metadata.id.clone(),
        leaf_id: Some("source".into()),
        custom_type: "goal-state".into(),
        entry_id: Some("source".into()),
    };
    let notice = || ScopedEntry::Notice {
        key: "revision-1".into(),
        message: rpi_harness::messages::create_custom_message(
            "goal",
            rpi_ai::UserContent::Text("Active goal".into()),
            true,
            None,
            1,
        ),
    };
    clock.armed.store(true, Ordering::SeqCst);
    let left = a.clone();
    let lg = guard.clone();
    let ln = notice();
    let right = b.clone();
    let rg = guard.clone();
    let rn = notice();
    let l = tokio::spawn(async move { left.append_scoped_entry(&lg, ln).await });
    let r = tokio::spawn(async move { right.append_scoped_entry(&rg, rn).await });
    let (l, r) = tokio::join!(l, r);
    let (l, r) = (l.unwrap(), r.unwrap());
    clock.armed.store(false, Ordering::SeqCst);
    assert_eq!(usize::from(l.is_ok()) + usize::from(r.is_ok()), 1);
    let accepted = l.or(r).unwrap();
    assert_eq!(
        a.append_scoped_entry(&guard, notice()).await.unwrap(),
        accepted
    );
    assert_eq!(
        b.append_scoped_entry(&guard, notice()).await.unwrap(),
        accepted
    );
    let reopened = first_repo.open(&metadata).await.unwrap();
    assert_eq!(
        reopened.get_lanes().await.unwrap()[0].leaf_id.as_deref(),
        Some("source")
    );
    assert_eq!(
        reopened
            .find_records(&RecordQuery {
                record_type: Some("queue_enqueued"),
                ..Default::default()
            })
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(reopened.get_entry(&accepted).await.unwrap().is_none());
}
