//! Pin the recovery query, not only its final answer: old full-queue reads fail.
use rpi_agent::AgentMessage;
use rpi_ai::types::{UserContent, UserMessage};
use rpi_harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
use rpi_harness::{
    agent_harness::{AgentHarness, AgentLane},
    error::{SessionError, SessionResult},
    session::{types::*, DefaultIdGenerator, Session, SqliteSessionRepo, SystemClock},
    types::AgentHarnessOptions,
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

struct Probe {
    inner: Arc<dyn SessionStorage>,
    queue_reads: Mutex<Vec<(String, usize)>>,
    cancellation_batches: Mutex<Vec<usize>>,
    fail_cancellation: AtomicBool,
    fail: AtomicBool,
}
#[async_trait::async_trait]
impl SessionStorage for Probe {
    fn metadata(&self) -> SessionMetadata {
        self.inner.metadata()
    }
    fn is_ephemeral(&self) -> bool {
        self.inner.is_ephemeral()
    }
    async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
        self.inner.get_lanes().await
    }
    async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
        self.inner.create_lane(lane, at).await
    }
    async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
        self.inner.move_lane(lane, to).await
    }
    async fn append_progress(
        &self,
        records: Vec<AssistantFrameRecord>,
    ) -> SessionResult<Vec<AssistantFrameRecord>> {
        self.inner.append_progress(records).await
    }
    async fn read_progress(&self, run_id: &str) -> SessionResult<Vec<AssistantFrameRecord>> {
        self.inner.read_progress(run_id).await
    }
    async fn prune_completed_progress(&self) -> SessionResult<usize> {
        self.inner.prune_completed_progress().await
    }
    async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
        self.inner.append_entry(entry, lane).await
    }
    async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        self.inner.append_record(record).await
    }
    async fn append_queue_cancellations(
        &self,
        records: Vec<LaneRecord>,
    ) -> SessionResult<Vec<LaneRecord>> {
        self.cancellation_batches
            .lock()
            .unwrap()
            .push(records.len());
        if self.fail_cancellation.load(Ordering::Relaxed) {
            return Err(SessionError::storage("cancellation batch rejected"));
        }
        self.inner.append_queue_cancellations(records).await
    }
    async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
        self.inner.get_entry(id).await
    }
    async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
        self.inner.find_entries(query).await
    }
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<Entry>> {
        self.inner
            .find_entries_on_branch(query, bounds, start)
            .await
    }
    async fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        // Startup uses typed queries only. Never permit an unfiltered "fallback".
        assert!(
            query.record_type.is_some(),
            "recovery must not load every record"
        );
        if matches!(
            query.record_type,
            Some("queue_enqueued" | "queue_cancelled")
        ) && query.target_entry_id.is_none()
            && query.queue_kind.is_none()
        {
            assert_eq!(query.record_type, Some("queue_enqueued"));
            assert!(
                query.pending_queue_only,
                "must filter before copying queue payloads"
            );
            assert_eq!(query.order, Some(EntryOrder::OldestFirst));
            assert!(query.limit.is_none(), "all pending work must be restored");
            let lane = query.lane.clone().expect("recovery must pin a lane");
            if self.fail.load(Ordering::Relaxed) {
                return Err(SessionError::storage("queue read rejected"));
            }
            let records = self.inner.find_records(query).await?;
            self.queue_reads.lock().unwrap().push((lane, records.len()));
            Ok(records)
        } else {
            self.inner.find_records(query).await
        }
    }
    async fn record_page(&self, query: &RecordPageQuery) -> SessionResult<RecordPage> {
        self.inner.record_page(query).await
    }
    async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> SessionResult<Vec<OperationStartedRecord>> {
        self.inner.find_open_operations(lane, limit).await
    }
    async fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
        self.inner.get_log(options).await
    }
    async fn get_name(&self) -> SessionResult<Option<String>> {
        self.inner.get_name().await
    }
    async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
        self.inner.set_name(name).await
    }
    async fn get_label(&self, id: &str) -> SessionResult<Option<String>> {
        self.inner.get_label(id).await
    }
    async fn set_label(&self, id: &str, label: Option<&str>) -> SessionResult<()> {
        self.inner.set_label(id, label).await
    }
    async fn get_stats(&self) -> SessionResult<SessionStats> {
        self.inner.get_stats().await
    }
    async fn drain(&self) {
        self.inner.drain().await
    }
}
fn base(id: &str, lane: &str) -> RecordBase {
    RecordBase {
        id: id.into(),
        lane: lane.into(),
        seq: 0,
        timestamp: 0,
    }
}
fn entry(id: &str, text: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.into(),
        kind: ProvisionedKind::Message {
            message: AgentMessage::User(UserMessage::new(UserContent::Text(text.into()), 0)),
            terminate: None,
        },
    }
}
async fn enqueue(store: &dyn SessionStorage, id: &str, lane: &str, kind: QueueKind, text: &str) {
    let full = provisioned_into_entry(entry(id, text), 0, None, 0);
    let mut target = full.to_flat_json();
    let object = target.as_object_mut().unwrap();
    for key in ["seq", "parentId", "timestamp"] {
        object.remove(key);
    }
    store
        .append_record(LaneRecord::QueueEnqueued(QueueEnqueuedRecord {
            base: base(&format!("enqueue-{lane}-{id}"), lane),
            queue: kind,
            run_id: None,
            target,
        }))
        .await
        .unwrap();
}
async fn cancel(store: &dyn SessionStorage, id: &str, lane: &str) {
    store
        .append_record(LaneRecord::QueueCancelled(QueueCancelledRecord {
            base: base(&format!("cancel-{lane}-{id}"), lane),
            entry_id: id.into(),
            run_id: None,
        }))
        .await
        .unwrap();
}
async fn fixture(store: &dyn SessionStorage) {
    store.create_lane("side", None).await.unwrap();
    enqueue(store, "first", "main", QueueKind::Steer, "first").await;
    for i in 0..128 {
        let id = format!("retired-{i}");
        enqueue(
            store,
            &id,
            "main",
            QueueKind::Steer,
            &"retired payload ".repeat(128),
        )
        .await;
        cancel(store, &id, "main").await;
    }
    enqueue(store, "second", "main", QueueKind::FollowUp, "second").await;
    enqueue(
        store,
        "already",
        "main",
        QueueKind::Steer,
        "already committed",
    )
    .await;
    store
        .append_entry(entry("already", "already committed"), "main")
        .await
        .unwrap();
    // The same target identity in another lane must not cancel the main item.
    enqueue(store, "shared", "main", QueueKind::Steer, "third").await;
    enqueue(store, "shared", "side", QueueKind::Steer, "side cancelled").await;
    cancel(store, "shared", "side").await;
    enqueue(
        store,
        "side-only",
        "side",
        QueueKind::FollowUp,
        "side survives",
    )
    .await;
    // Unrelated historical progress is large but must never be requested.
    for i in 0..16 {
        store
            .append_record(LaneRecord::AssistantFrame(AssistantFrameRecord {
                base: base(&format!("frame-{i}"), "main"),
                run_id: "old-run".into(),
                stream_index: 0,
                op: AssistantFrameOp::Append,
                frame: Some(serde_json::json!({"delta":"unrelated".repeat(1024)})),
            }))
            .await
            .unwrap();
    }
}
async fn check(store: Arc<dyn SessionStorage>) {
    fixture(store.as_ref()).await;
    let probe = Arc::new(Probe {
        inner: store,
        queue_reads: Mutex::new(Vec::new()),
        cancellation_batches: Mutex::new(Vec::new()),
        fail_cancellation: AtomicBool::new(false),
        fail: AtomicBool::new(false),
    });
    let session = Session::new(probe.clone(), None);
    let harness = AgentHarness::create(AgentHarnessOptions {
        session: session.clone(),
        allow_existing_session: true,
        ..Default::default()
    })
    .await
    .unwrap();
    let queued = harness.lane("main").queued_messages().await.unwrap();
    assert_eq!(
        queued
            .steering
            .iter()
            .map(|view| view.text.as_str())
            .collect::<Vec<_>>(),
        ["first", "third"]
    );
    assert_eq!(
        queued
            .follow_up
            .iter()
            .map(|view| view.text.as_str())
            .collect::<Vec<_>>(),
        ["second"]
    );
    assert_eq!(*probe.queue_reads.lock().unwrap(), [("main".into(), 3)]);
    harness.set_session(session.clone()).await.unwrap();
    let side = harness.lane("side").queued_messages().await.unwrap();
    assert!(side.steering.is_empty());
    assert_eq!(
        side.follow_up
            .iter()
            .map(|view| view.text.as_str())
            .collect::<Vec<_>>(),
        ["side survives"]
    );
    assert_eq!(probe.queue_reads.lock().unwrap().len(), 3);
    probe.fail.store(true, Ordering::Relaxed);
    assert!(harness.set_session(session.clone()).await.is_err());
    assert_eq!(
        harness.lane("main").queued_messages().await.unwrap(),
        queued,
        "failed switch must preserve live queue"
    );
    assert!(
        AgentHarness::create(AgentHarnessOptions {
            session,
            allow_existing_session: true,
            ..Default::default()
        })
        .await
        .is_err(),
        "read failure must not become an empty restored queue"
    );
    probe.fail.store(false, Ordering::Relaxed);
    let cancelled = harness.lane("main").cancel_queued("first").await.unwrap();
    assert_eq!(
        cancelled.outcome,
        rpi_harness::agent_harness::CancelQueuedOutcome::Cancelled
    );
    harness
        .set_session(harness.session().clone())
        .await
        .unwrap();
    assert_eq!(
        harness
            .lane("main")
            .queued_messages()
            .await
            .unwrap()
            .steering
            .iter()
            .map(|view| view.text.as_str())
            .collect::<Vec<_>>(),
        ["third"]
    );
    let next = harness
        .next_run(UserMessage::new("next survives", 0).into())
        .await
        .unwrap();
    let before_clear = harness.queued_messages().await.unwrap();
    assert_eq!(before_clear.steering[0].text, "third");
    assert_eq!(before_clear.follow_up[0].text, "second");
    probe.fail_cancellation.store(true, Ordering::Relaxed);
    assert!(harness.clear_queue().await.is_err());
    assert_eq!(harness.queued_messages().await.unwrap(), before_clear);
    harness
        .set_session(harness.session().clone())
        .await
        .unwrap();
    assert_eq!(harness.queued_messages().await.unwrap(), before_clear);
    probe.fail_cancellation.store(false, Ordering::Relaxed);
    assert_eq!(harness.clear_queue().await.unwrap(), before_clear);
    assert_eq!(*probe.cancellation_batches.lock().unwrap(), [1, 2, 2]);
    harness
        .set_session(harness.session().clone())
        .await
        .unwrap();
    assert!(harness.queued_messages().await.unwrap().is_empty());
    let pending = harness
        .session()
        .find_records(&RecordQuery {
            lane: Some("main".into()),
            record_type: Some("queue_enqueued"),
            pending_queue_only: true,
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(pending.len(), 1, "clear must preserve next-run work");
    let LaneRecord::QueueEnqueued(pending) = &pending[0] else {
        panic!("expected enqueue")
    };
    assert_eq!(pending.queue, QueueKind::NextRun);
    assert_eq!(pending.target["id"].as_str(), Some(next.entry_id.as_str()));
    let side = harness.lane("side").queued_messages().await.unwrap();
    assert_eq!(side.follow_up[0].text, "side survives");
}

#[tokio::test]
async fn restore_memory_selects_only_pending_payloads() {
    check(AgentHarnessOptions::default().session.storage()).await;
}
#[tokio::test]
async fn restore_sqlite_selects_only_pending_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let repo = SqliteSessionRepo::open(dir.path().join("sessions.db")).unwrap();
    let store = repo.create(&SessionCreateOptions::default()).await.unwrap();
    check(Arc::new(store)).await;
}
#[tokio::test]
async fn restore_jsonl_selects_only_pending_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let store = JsonlSessionStorage::create(
        fs,
        "session.jsonl",
        JsonlV4Header::new("queue".into(), 1, "/".into(), None, None, None),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    check(Arc::new(store)).await;
}
