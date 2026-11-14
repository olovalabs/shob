//! Fault probes for live error recovery and stable fallback commit identities.
#[path = "support/final_assistant_fallback.rs"]
mod final_assistant_fallback;
#[path = "support/tool_execution_barrier.rs"]
mod tool_execution_barrier;
#[path = "support/tool_result_fallback.rs"]
mod tool_result_fallback;

use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};

use rpi_agent::AgentMessage;
use rpi_ai::providers::faux::{faux_assistant_message, FauxProvider, FauxScript};
use rpi_ai::types::{AssistantMessageEvent, StopReason};
use rpi_ai::{AssistantMessageEventStream, Provider};
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_harness::error::{SessionError, SessionResult};
use rpi_harness::session::types::*;
use rpi_harness::session::Session;
use rpi_harness::types::AgentHarnessOptions;
use tokio::sync::Notify;

const TEXT: &str = "The final queued delta must survive a broken provider stream.";

/// Drop without Done/Error: the agent loop returns Err, not an error message.
struct TruncatedProvider(Arc<FauxProvider>);

#[async_trait::async_trait]
impl Provider for TruncatedProvider {
    fn id(&self) -> &str {
        self.0.id()
    }
    fn models(&self) -> &[rpi_ai::Model] {
        self.0.models()
    }
    async fn stream_simple(
        &self,
        _model: &rpi_ai::Model,
        _context: &rpi_ai::types::Context,
        _options: &rpi_ai::SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let (mut producer, stream) = rpi_ai::event_stream::create_assistant_message_event_stream();
        let empty = Arc::new(faux_assistant_message("", StopReason::Pending));
        assert!(
            producer
                .push(AssistantMessageEvent::Start {
                    partial: empty.clone(),
                })
                .await
        );
        assert!(
            producer
                .push(AssistantMessageEvent::TextStart {
                    content_index: 0,
                    partial: empty,
                })
                .await
        );
        assert!(
            producer
                .push(AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: TEXT.into(),
                    partial: Arc::new(faux_assistant_message(TEXT, StopReason::Pending)),
                })
                .await
        );
        producer.close();
        stream
    }
}

/// Pause before the inner store's write lock. Reads see only committed frames,
/// even if the timed frame checkpoint has already started. Thus scheduling or a
/// backend's incidental read/write lock cannot hide a missing flush barrier.
struct ProbedStorage {
    inner: Arc<dyn SessionStorage>,
    pause: AtomicBool,
    entered: Notify,
    release: Notify,
    reads: AtomicUsize,
    fail_append: AtomicBool,
    fail_assistant: AtomicBool,
    fail_assistants: Mutex<usize>,
    assistant_attempts: Mutex<Vec<String>>,
    fail_binding: AtomicBool,
    fail_clear: AtomicBool,
    fail_results: Mutex<usize>,
    result_attempts: Mutex<Vec<String>>,
    fail_start: AtomicBool,
    pause_start: AtomicBool,
    start_entered: Notify,
    start_release: Notify,
}

impl ProbedStorage {
    fn new(inner: Arc<dyn SessionStorage>) -> Self {
        Self {
            inner,
            pause: AtomicBool::new(true),
            entered: Notify::new(),
            release: Notify::new(),
            reads: AtomicUsize::new(0),
            fail_append: AtomicBool::new(false),
            fail_assistant: AtomicBool::new(false),
            fail_assistants: Mutex::new(0),
            assistant_attempts: Mutex::new(Vec::new()),
            fail_binding: AtomicBool::new(false),
            fail_clear: AtomicBool::new(false),
            fail_results: Mutex::new(0),
            result_attempts: Mutex::new(Vec::new()),
            fail_start: AtomicBool::new(false),
            pause_start: AtomicBool::new(false),
            start_entered: Notify::new(),
            start_release: Notify::new(),
        }
    }
}

#[async_trait::async_trait]
impl SessionStorage for ProbedStorage {
    async fn append_progress(
        &self,
        records: Vec<AssistantFrameRecord>,
    ) -> SessionResult<Vec<AssistantFrameRecord>> {
        if self.pause.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        if self.fail_append.swap(false, Ordering::SeqCst) {
            return Err(SessionError::storage("controlled progress append failure"));
        }
        if records
            .iter()
            .any(|record| record.op == AssistantFrameOp::ClearRun)
            && self.fail_clear.swap(false, Ordering::SeqCst)
        {
            return Err(SessionError::storage("controlled retirement failure"));
        }
        self.inner.append_progress(records).await
    }
    async fn read_progress(&self, run_id: &str) -> SessionResult<Vec<AssistantFrameRecord>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.read_progress(run_id).await
    }
    async fn prune_completed_progress(&self) -> SessionResult<usize> {
        self.inner.prune_completed_progress().await
    }
    fn metadata(&self) -> SessionMetadata {
        self.inner.metadata()
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
    async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
        if matches!(
            &entry.kind,
            ProvisionedKind::Message {
                message: AgentMessage::ToolResult(_),
                ..
            }
        ) {
            self.result_attempts.lock().unwrap().push(entry.id.clone());
            let fail = {
                let mut left = self.fail_results.lock().unwrap();
                if *left == 0 {
                    false
                } else {
                    *left -= 1;
                    true
                }
            };
            if fail {
                return Err(SessionError::storage(
                    "controlled tool result entry failure",
                ));
            }
        }
        if matches!(
            &entry.kind,
            ProvisionedKind::Message {
                message: AgentMessage::Assistant(_),
                ..
            }
        ) {
            self.assistant_attempts
                .lock()
                .unwrap()
                .push(entry.id.clone());
            let fail = {
                let mut left = self.fail_assistants.lock().unwrap();
                if *left == 0 {
                    false
                } else {
                    *left -= 1;
                    true
                }
            };
            if self.fail_assistant.swap(false, Ordering::SeqCst) || fail {
                return Err(SessionError::storage("controlled recovery entry failure"));
            }
        }
        self.inner.append_entry(entry, lane).await
    }
    async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
        if matches!(&record, LaneRecord::AssistantFrame(frame)
            if frame.op == AssistantFrameOp::CommitStream)
            && self.fail_binding.swap(false, Ordering::SeqCst)
        {
            return Err(SessionError::storage("controlled identity binding failure"));
        }
        if matches!(record, LaneRecord::ToolStarted(_)) {
            let fail = self.fail_start.swap(false, Ordering::SeqCst);
            if self.pause_start.swap(false, Ordering::SeqCst) {
                self.start_entered.notify_one();
                self.start_release.notified().await;
            }
            if fail {
                return Err(SessionError::storage("controlled tool_started failure"));
            }
        }
        self.inner.append_record(record).await
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
        self.inner.find_records(query).await
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
}

async fn fixture() -> (AgentHarness, Arc<ProbedStorage>) {
    let defaults = AgentHarnessOptions::default();
    let storage = Arc::new(ProbedStorage::new(defaults.session.storage()));
    let inner = FauxProvider::new(FauxScript::new());
    let harness = AgentHarness::create(AgentHarnessOptions {
        model: inner.default_model().clone(),
        models: vec![Arc::new(TruncatedProvider(inner))],
        session: Session::new(storage.clone(), None),
        ..defaults
    })
    .await
    .unwrap();
    (harness, storage)
}

async fn records(storage: &ProbedStorage, record_type: &'static str) -> Vec<LaneRecord> {
    storage
        .find_records(&RecordQuery {
            record_type: Some(record_type),
            ..Default::default()
        })
        .await
        .unwrap()
}

async fn assistant_entries(storage: &ProbedStorage) -> Vec<MessageEntry> {
    storage
        .find_entries(&EntryQuery::default())
        .await
        .unwrap()
        .into_iter()
        .filter_map(|entry| match entry {
            Entry::Message(message) if matches!(message.message, AgentMessage::Assistant(_)) => {
                Some(message)
            }
            _ => None,
        })
        .collect()
}

async fn assert_partial(storage: &ProbedStorage) -> String {
    let entries = assistant_entries(storage).await;
    assert_eq!(entries.len(), 1, "recover exactly one assistant");
    let AgentMessage::Assistant(message) = &entries[0].message else {
        unreachable!()
    };
    assert_eq!(message.stop_reason, StopReason::Error);
    let rpi_ai::types::Content::Text(content) = &message.content[0] else {
        panic!("expected recovered text")
    };
    assert_eq!(content.text, TEXT);
    entries[0].base.id.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loop_error_waits_for_queued_frames_before_recovery_and_terminal() {
    let (harness, storage) = fixture().await;
    let baseline_reads = storage.reads.load(Ordering::SeqCst);
    let mut run = tokio::spawn(async move { harness.prompt_text("answer", vec![]).await });
    tokio::select! {
        _ = storage.entered.notified() => {}
        result = &mut run => panic!("loop error did not wait for the frame writer: {result:?}"),
        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => panic!("writer did not start"),
    }
    assert_eq!(storage.reads.load(Ordering::SeqCst), baseline_reads);
    assert!(records(&storage, "operation_finished").await.is_empty());
    assert!(!run.is_finished());
    storage.release.notify_one();
    let result = run.await.unwrap().unwrap();
    let HarnessRunOutcome::Failed { error, .. } = result.outcome else {
        panic!("a truncated stream must fail")
    };
    assert_eq!(error.code, "agent_error");
    assert!(error.message.contains("without a terminal event"));
    assert_partial(&storage).await;
    assert!(storage
        .read_progress(&result.run_id)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(records(&storage, "operation_finished").await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_flush_failure_leaves_the_operation_open_and_emits_no_run_end() {
    let (harness, storage) = fixture().await;
    storage.fail_append.store(true, Ordering::SeqCst);
    let ended = Arc::new(AtomicUsize::new(0));
    let counter = ended.clone();
    let _listener = harness
        .events()
        .on::<rpi_harness::events::RunEndEvent, _>(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
    let run = tokio::spawn(async move { harness.prompt_text("answer", vec![]).await });
    // Release even if a broken implementation has already returned. No sleeps
    // are needed: Notify remembers the permit for the writer's first append.
    storage.release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.is_err(),
        "a failed flush must not finalize a run: {result:?}"
    );
    assert!(records(&storage, "operation_finished").await.is_empty());
    assert_eq!(
        storage
            .find_open_operations("main", None)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(ended.load(Ordering::SeqCst), 0);
    assert!(assistant_entries(&storage).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_entry_failure_keeps_payload_and_reuses_the_durable_binding_on_reopen() {
    let (harness, storage) = fixture().await;
    storage.fail_assistant.store(true, Ordering::SeqCst);
    storage.release.notify_one();
    let result = harness.prompt_text("answer", vec![]).await;
    assert!(
        result.is_err(),
        "recovery must propagate entry failure: {result:?}"
    );
    assert!(records(&storage, "operation_finished").await.is_empty());
    let open = storage.find_open_operations("main", None).await.unwrap();
    assert_eq!(open.len(), 1);
    let run_id = &open[0].base.id;
    assert!(!storage.read_progress(run_id).await.unwrap().is_empty());
    let bindings = records(&storage, "assistant_frame").await;
    let reserved_id = bindings
        .iter()
        .find_map(|record| match record {
            LaneRecord::AssistantFrame(frame) if frame.op == AssistantFrameOp::CommitStream => {
                frame
                    .frame
                    .as_ref()
                    .and_then(|value| value.get("entryId"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            }
            _ => None,
        })
        .expect("recovery reserves an entry before attempting to append it");
    drop(harness);
    let options = || AgentHarnessOptions {
        session: Session::new(storage.clone(), None),
        allow_existing_session: true,
        ..Default::default()
    };
    drop(AgentHarness::create(options()).await.unwrap());
    assert_eq!(assert_partial(&storage).await, reserved_id);
    assert!(storage.read_progress(run_id).await.unwrap().is_empty());
    drop(AgentHarness::create(options()).await.unwrap());
    assert_eq!(assert_partial(&storage).await, reserved_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_error_path_retirement_is_retried_from_terminal_proof_without_replay() {
    let (harness, storage) = fixture().await;
    storage.fail_clear.store(true, Ordering::SeqCst);
    storage.release.notify_one();
    let result = harness.prompt_text("answer", vec![]).await.unwrap();
    assert!(matches!(result.outcome, HarnessRunOutcome::Failed { .. }));
    assert!(
        !storage.fail_clear.load(Ordering::SeqCst),
        "the error path must actually attempt retirement"
    );
    let entry_id = assert_partial(&storage).await;
    assert!(!storage
        .read_progress(&result.run_id)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(records(&storage, "operation_finished").await.len(), 1);
    drop(harness);
    drop(
        AgentHarness::create(AgentHarnessOptions {
            session: Session::new(storage.clone(), None),
            allow_existing_session: true,
            ..Default::default()
        })
        .await
        .unwrap(),
    );
    assert!(storage
        .read_progress(&result.run_id)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(assert_partial(&storage).await, entry_id);
}
