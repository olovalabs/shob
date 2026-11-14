//! Bounded recovery classifier contract. The probe decodes only selected rows,
//! modeling a cold payload backend without touching JSONL/runtime internals.
use super::*;
use crate::error::SessionResult;
use crate::session::memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage};
use crate::session::types::*;
use std::sync::atomic::AtomicBool;

#[derive(Default)]
struct Reads {
    batches: Vec<(String, Option<EntryCursor>, usize)>,
    entry_rows: usize,
    queue_rows: usize,
    decoded_bytes: usize,
}

struct Probe {
    inner: InMemorySessionStorage,
    reads: Mutex<Reads>,
    fail_leaf: AtomicBool,
    fail_branch_call: AtomicUsize,
    branch_calls: AtomicUsize,
    fail_queue: AtomicBool,
    move_after_read: AtomicBool,
}

impl Probe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: InMemorySessionStorage::new(
                SessionMetadata {
                    id: "idle-probe".into(),
                    created_at: 0,
                    parent_session_id: None,
                },
                Arc::new(FakeClock::new()),
                Arc::new(CounterIdGenerator::new()),
            ),
            reads: Mutex::new(Reads::default()),
            fail_leaf: AtomicBool::new(false),
            fail_branch_call: AtomicUsize::new(0),
            branch_calls: AtomicUsize::new(0),
            fail_queue: AtomicBool::new(false),
            move_after_read: AtomicBool::new(false),
        })
    }

    fn session(self: &Arc<Self>) -> Session {
        Session::new(self.clone(), None)
    }

    async fn put(&self, lane: &str, id: &str, kind: ProvisionedKind) {
        self.inner
            .append_entry(
                ProvisionedEntry {
                    id: id.into(),
                    kind,
                },
                lane,
            )
            .await
            .unwrap();
    }

    async fn enqueue(&self, lane: &str, record_id: &str, id: &str, queue: QueueKind) {
        let target = provisioned_into_entry(
            ProvisionedEntry {
                id: id.into(),
                kind: user("queued payload"),
            },
            0,
            None,
            0,
        )
        .to_flat_json();
        self.inner
            .append_record(LaneRecord::QueueEnqueued(QueueEnqueuedRecord {
                base: RecordBase {
                    id: record_id.into(),
                    seq: 0,
                    timestamp: 0,
                    lane: lane.into(),
                },
                queue,
                run_id: None,
                target,
            }))
            .await
            .unwrap();
    }

    fn reset_reads(&self) {
        *self.reads.lock().unwrap() = Reads::default();
        self.branch_calls.store(0, Ordering::Relaxed);
    }
}

#[async_trait::async_trait]
impl SessionStorage for Probe {
    fn metadata(&self) -> SessionMetadata {
        self.inner.metadata()
    }
    async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
        if self.fail_leaf.load(Ordering::Relaxed) {
            return Err(SessionError::storage("leaf read rejected"));
        }
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
    async fn read_progress(&self, run: &str) -> SessionResult<Vec<AssistantFrameRecord>> {
        self.inner.read_progress(run).await
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
    async fn get_entry(&self, _id: &str) -> SessionResult<Option<Entry>> {
        panic!("classifier must use bounded branch reads, not per-entry/full-history fallback")
    }
    async fn find_entries(&self, _query: &EntryQuery) -> SessionResult<Vec<Entry>> {
        panic!("classifier must not read session-wide history")
    }
    async fn find_entries_on_branch(
        &self,
        query: &EntryQuery,
        bounds: &BranchBounds,
        start: &str,
    ) -> SessionResult<Vec<Entry>> {
        assert_eq!(query.order, Some(EntryOrder::NewestFirst));
        assert!(matches!(query.limit, Some(1..=16)));
        assert_eq!(
            bounds.start.as_deref(),
            Some(start),
            "pin the same leaf for every batch"
        );
        let call = self.branch_calls.fetch_add(1, Ordering::Relaxed) + 1;
        if self.fail_branch_call.load(Ordering::Relaxed) == call {
            return Err(SessionError::storage("branch read rejected"));
        }
        let entries = self
            .inner
            .find_entries_on_branch(query, bounds, start)
            .await?;
        let entries = {
            let mut reads = self.reads.lock().unwrap();
            if let Some((previous_start, previous_cursor, _)) = reads.batches.last() {
                assert_eq!(previous_start, start);
                assert!(query.cursor.is_some());
                if let Some(previous) = previous_cursor {
                    assert!(query.cursor.unwrap().after_seq < previous.after_seq);
                }
            }
            reads
                .batches
                .push((start.into(), query.cursor, entries.len()));
            reads.entry_rows += entries.len();
            // Simulate lazy full-payload decode at the storage boundary. Metadata
            // selection happens above; unrelated historical rows are never decoded.
            entries
                .into_iter()
                .map(|entry| {
                    let bytes = serde_json::to_vec(&entry).unwrap();
                    reads.decoded_bytes += bytes.len();
                    serde_json::from_slice(&bytes).unwrap()
                })
                .collect()
        };
        if self.move_after_read.swap(false, Ordering::Relaxed) {
            self.put("main", "concurrent-assistant", assistant()).await;
        }
        Ok(entries)
    }
    async fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
        assert_eq!(query.record_type, Some("queue_enqueued"));
        assert!(query.lane.is_some());
        assert!(
            query.target_entry_id.is_some(),
            "never hydrate all historical queue payloads"
        );
        assert_eq!(
            query.order,
            Some(EntryOrder::OldestFirst),
            "old map semantics: oldest duplicate wins"
        );
        assert_eq!(query.limit, Some(1));
        assert!(
            !query.pending_queue_only,
            "committed messages still need delivery classification"
        );
        assert!(
            query.queue_kind.is_none(),
            "do not filter away a duplicate's winning kind"
        );
        if self.fail_queue.load(Ordering::Relaxed) {
            return Err(SessionError::storage("queue read rejected"));
        }
        let records = self.inner.find_records(query).await?;
        let mut reads = self.reads.lock().unwrap();
        reads.queue_rows += records.len();
        Ok(records
            .into_iter()
            .map(|record| {
                let bytes = serde_json::to_vec(&record).unwrap();
                reads.decoded_bytes += bytes.len();
                serde_json::from_slice(&bytes).unwrap()
            })
            .collect())
    }
    async fn record_page(&self, _query: &RecordPageQuery) -> SessionResult<RecordPage> {
        panic!("no full record-page fallback")
    }
    async fn find_open_operations(
        &self,
        lane: &str,
        limit: Option<usize>,
    ) -> SessionResult<Vec<OperationStartedRecord>> {
        self.inner.find_open_operations(lane, limit).await
    }
    async fn get_log(&self, _options: &LogOptions) -> SessionResult<Vec<LogItem>> {
        panic!("no full log fallback")
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
}

fn user(text: &str) -> ProvisionedKind {
    ProvisionedKind::Message {
        message: AgentMessage::User(UserMessage::new(text, 0)),
        terminate: None,
    }
}
fn assistant() -> ProvisionedKind {
    ProvisionedKind::Message {
        message: AgentMessage::Assistant(Box::new(AssistantMessage::empty(
            rpi_ai::Api::Faux,
            "probe",
            "m",
            0,
        ))),
        terminate: None,
    }
}
fn tool(terminate: Option<bool>) -> ProvisionedKind {
    ProvisionedKind::Message {
        message: AgentMessage::ToolResult(Box::new(rpi_ai::types::ToolResultMessage {
            role: rpi_ai::types::ToolResultRole,
            tool_call_id: "call".into(),
            tool_name: "probe".into(),
            content: vec![Content::text("result")],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: 0,
        })),
        terminate,
    }
}
fn custom() -> ProvisionedKind {
    ProvisionedKind::Custom {
        custom_type: "extension".into(),
        data: None,
    }
}

#[tokio::test]
async fn bounded_idle_classification_preserves_terminal_and_queue_kinds() {
    for (kind, queue, expected) in [
        (tool(None), None, true),
        (tool(Some(false)), None, true),
        (tool(Some(true)), None, false),
        (assistant(), None, false),
        (
            ProvisionedKind::Message {
                message: AgentMessage::Custom(rpi_agent::message::CustomMessage::new(
                    "assistantPending",
                    vec![],
                    serde_json::json!({}),
                    0,
                )),
                terminate: None,
            },
            None,
            false,
        ),
        (user("prompt"), None, false),
        (user("steer"), Some(QueueKind::Steer), true),
        (user("follow"), Some(QueueKind::FollowUp), true),
        (user("next"), Some(QueueKind::NextRun), false),
        (user("context"), Some(QueueKind::Context), false),
        (
            ProvisionedKind::ThinkingLevel {
                thinking_level: "off".into(),
            },
            None,
            false,
        ),
    ] {
        let probe = Probe::new();
        if let Some(queue) = queue {
            probe.enqueue("main", "q", "tip", queue).await;
        }
        probe.put("main", "tip", kind).await;
        probe.put("main", "extension", custom()).await;
        assert_eq!(
            AgentHarness::lane_is_resumable_checked(&probe.session(), "main")
                .await
                .unwrap(),
            expected
        );
        probe.reset_reads();
        assert_eq!(
            AgentHarness::lane_is_resumable(&probe.session(), "main").await,
            expected
        );
    }
    let empty = Probe::new();
    assert!(
        !AgentHarness::lane_is_resumable_checked(&empty.session(), "main")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn bounded_idle_skips_multiple_batches_of_custom_and_context_on_pinned_leaf() {
    let probe = Probe::new();
    probe.put("main", "tool", tool(None)).await;
    for i in 0..40 {
        let id = format!("context-{i}");
        probe
            .enqueue("main", &format!("q-{i}"), &id, QueueKind::Context)
            .await;
        probe.put("main", &id, user("silent")).await;
        probe.put("main", &format!("custom-{i}"), custom()).await;
    }
    probe.move_after_read.store(true, Ordering::Relaxed);
    assert!(
        AgentHarness::lane_is_resumable_checked(&probe.session(), "main")
            .await
            .unwrap()
    );
    {
        let reads = probe.reads.lock().unwrap();
        assert_eq!(reads.batches.len(), 6);
        assert_eq!(reads.entry_rows, 81);
        assert_eq!(reads.queue_rows, 40);
    }
    probe.reset_reads();
    // The next independent check sees the concurrent assistant, not the old leaf.
    assert!(
        !AgentHarness::lane_is_resumable_checked(&probe.session(), "main")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn bounded_idle_exhausts_custom_history_and_is_lane_scoped() {
    let probe = Probe::new();
    probe.inner.create_lane("side", None).await.unwrap();
    probe.put("main", "main-tool", tool(None)).await;
    for i in 0..33 {
        probe.put("side", &format!("custom-{i}"), custom()).await;
    }
    assert!(
        !AgentHarness::lane_is_resumable_checked(&probe.session(), "side")
            .await
            .unwrap()
    );
    assert_eq!(probe.reads.lock().unwrap().entry_rows, 33);
    assert_eq!(probe.branch_calls.load(Ordering::Relaxed), 4); // includes empty exhaustion batch
}

#[tokio::test]
async fn bounded_idle_preserves_oldest_duplicate_target_kind_and_lane_filter() {
    for (oldest, newest, expected) in [
        (QueueKind::Context, QueueKind::Steer, false),
        (QueueKind::Steer, QueueKind::Context, true),
        (QueueKind::NextRun, QueueKind::FollowUp, false),
    ] {
        let probe = Probe::new();
        probe.inner.create_lane("side", None).await.unwrap();
        probe
            .enqueue("side", "unrelated", "tip", QueueKind::FollowUp)
            .await;
        probe.enqueue("main", "old", "tip", oldest).await;
        probe.enqueue("main", "new", "tip", newest).await;
        probe.put("main", "tip", user("queued")).await;
        assert_eq!(
            AgentHarness::lane_is_resumable_checked(&probe.session(), "main")
                .await
                .unwrap(),
            expected
        );
        assert_eq!(probe.reads.lock().unwrap().queue_rows, 1);
    }
}

#[tokio::test]
async fn bounded_idle_read_errors_are_strict_but_legacy_is_best_effort() {
    for failure in ["leaf", "first-batch", "later-batch", "queue"] {
        let probe = Probe::new();
        probe.put("main", "tip", tool(None)).await;
        if failure == "later-batch" {
            for i in 0..17 {
                probe.put("main", &format!("custom-{i}"), custom()).await;
            }
        }
        match failure {
            "leaf" => probe.fail_leaf.store(true, Ordering::Relaxed),
            "first-batch" => probe.fail_branch_call.store(1, Ordering::Relaxed),
            "later-batch" => probe.fail_branch_call.store(2, Ordering::Relaxed),
            "queue" => probe.fail_queue.store(true, Ordering::Relaxed),
            _ => unreachable!(),
        }
        assert!(
            AgentHarness::lane_is_resumable_checked(&probe.session(), "main")
                .await
                .is_err(),
            "{failure}"
        );
        probe.reset_reads();
        // Queue read failure historically meant an empty kind map. A tool
        // result therefore remains resumable, unlike a failed branch/leaf read.
        assert_eq!(
            AgentHarness::lane_is_resumable(&probe.session(), "main").await,
            failure == "queue",
            "{failure}"
        );
    }
    let probe = Probe::new();
    probe.enqueue("main", "q", "tip", QueueKind::Steer).await;
    probe.put("main", "tip", user("queued")).await;
    probe.fail_queue.store(true, Ordering::Relaxed);
    assert!(!AgentHarness::lane_is_resumable(&probe.session(), "main").await);
}

#[tokio::test]
async fn bounded_idle_long_history_decodes_only_one_batch_and_target_queue_payload() {
    let probe = Probe::new();
    let payload = "unrelated historical payload ".repeat(256);
    for i in 0..2048 {
        let id = format!("old-{i}");
        probe.put("main", &id, user(&payload)).await;
        probe
            .enqueue("main", &format!("q-{i}"), &id, QueueKind::Steer)
            .await;
    }
    probe
        .enqueue("main", "tip-queue", "tip", QueueKind::FollowUp)
        .await;
    probe.put("main", "tip", user("follow up")).await;
    assert!(
        AgentHarness::lane_is_resumable_checked(&probe.session(), "main")
            .await
            .unwrap()
    );
    let reads = probe.reads.lock().unwrap();
    assert_eq!(reads.batches.len(), 1);
    assert_eq!(reads.entry_rows, 16);
    assert_eq!(reads.queue_rows, 1);
    assert!(
        reads.decoded_bytes < 150_000,
        "decoded {} bytes",
        reads.decoded_bytes
    );
}
