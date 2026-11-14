use rpi_agent::{tool_program::current, AgentError, AgentTool, AgentToolResult, ToolResultPartial};
use rpi_ai::{
    providers::faux::{FauxProvider, FauxScript},
    types::Tool,
};
use rpi_harness::{
    agent_harness::{AgentHarness, AgentLane},
    session::{types::*, Session},
    types::{AgentHarnessOptions, HarnessTool},
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio_util::sync::CancellationToken;
fn schema(name: &str) -> Tool {
    serde_json::from_value(json!({"name":name,"description":"test","parameters":{"type":"object"}}))
        .unwrap()
}
struct Program {
    schema: Tool,
    count: Arc<AtomicUsize>,
    pause: bool,
}
#[async_trait::async_trait]
impl AgentTool for Program {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "program"
    }
    fn execution_mode(&self) -> rpi_agent::ToolExecutionMode {
        rpi_agent::ToolExecutionMode::Sequential
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        let p = current().unwrap();
        let id = p
            .handle(json!({"op":"start","name":"effect","arguments":{}}))
            .await
            .unwrap()["requestId"]
            .clone();
        loop {
            let r = p.handle(json!({"op":"poll","requestId":id})).await.unwrap();
            if r["status"] != "running" {
                if r["isError"] == true || r["status"] == "failed" {
                    return Err(AgentError::tool(r.to_string()));
                }
                break;
            }
            tokio::task::yield_now().await;
        }
        if self.pause {
            std::future::pending::<()>().await;
        }
        Ok(AgentToolResult::text("program done"))
    }
}
struct Effect {
    schema: Tool,
    session: Session,
    count: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl AgentTool for Effect {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "effect"
    }
    async fn execute(
        &self,
        id: &str,
        _: Value,
        _: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let entries = self
            .session
            .find_entries(&EntryQuery::default())
            .await
            .unwrap();
        let encoded = serde_json::to_value(entries).unwrap().to_string();
        assert!(encoded.contains("rpi.tool_program.intent.v1"));
        assert!(encoded.contains(id));
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(AgentToolResult::text("effect"))
    }
}
async fn build(
    session: Session,
    programs: Arc<AtomicUsize>,
    effects: Arc<AtomicUsize>,
    pause: bool,
    safe: bool,
) -> AgentHarness {
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("codemode", json!({}))
            .with_text("done"),
    );
    let mut program = HarnessTool::new(Arc::new(Program {
        schema: schema("codemode"),
        count: programs,
        pause,
    }));
    if safe {
        program = program.with_replay(rpi_harness::types::ToolReplay::Safe);
    }
    AgentHarness::create(AgentHarnessOptions {
        session: session.clone(),
        allow_existing_session: true,
        model: provider.default_model().clone(),
        models: vec![provider],
        tools: vec![
            program,
            HarnessTool::new(Arc::new(Effect {
                schema: schema("effect"),
                session,
                count: effects,
            })),
        ],
        ..Default::default()
    })
    .await
    .unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoint_precedes_effect_without_fabricating_transcript_calls() {
    let session = AgentHarnessOptions::default().session;
    let programs = Arc::new(AtomicUsize::new(0));
    let effects = Arc::new(AtomicUsize::new(0));
    let h = build(session.clone(), programs, effects.clone(), false, false).await;
    h.prompt_text("run", vec![]).await.unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    let entries = session.find_entries(&EntryQuery::default()).await.unwrap();
    let results = entries
        .iter()
        .filter_map(|e| {
            if let Entry::Message(m) = e {
                if let rpi_agent::AgentMessage::ToolResult(r) = &m.message {
                    Some(r)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].tool_name, "codemode");
    assert!(!results[0].is_error);
    let records = session.find_records(&RecordQuery::default()).await.unwrap();
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r, LaneRecord::ToolStarted(_)))
            .count(),
        1
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_program_is_never_automatically_replayed() {
    let session = AgentHarnessOptions::default().session;
    let programs = Arc::new(AtomicUsize::new(0));
    let effects = Arc::new(AtomicUsize::new(0));
    let h = build(
        session.clone(),
        programs.clone(),
        effects.clone(),
        true,
        false,
    )
    .await;
    let runner = h.clone();
    let run = tokio::spawn(async move { runner.prompt_text("run", vec![]).await });
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while effects.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    run.abort();
    let _ = run.await;
    drop(h);
    for _ in 0..2 {
        let restored = build(
            session.clone(),
            programs.clone(),
            effects.clone(),
            false,
            false,
        )
        .await;
        drop(restored);
    }
    assert_eq!(programs.load(Ordering::SeqCst), 1);
    assert_eq!(effects.load(Ordering::SeqCst), 1);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_safe_parent_fails_closed_before_nested_effect() {
    let effects = Arc::new(AtomicUsize::new(0));
    let h = build(
        AgentHarnessOptions::default().session,
        Arc::new(AtomicUsize::new(0)),
        effects.clone(),
        false,
        true,
    )
    .await;
    let _ = h.prompt_text("run", vec![]).await;
    assert_eq!(effects.load(Ordering::SeqCst), 0);
}

use rpi_harness::error::{SessionError, SessionResult};
struct FaultStorage {
    inner: Arc<dyn SessionStorage>,
    marker: &'static str,
}
#[async_trait::async_trait]
impl SessionStorage for FaultStorage {
    fn metadata(&self) -> SessionMetadata {
        self.inner.metadata()
    }
    fn is_ephemeral(&self) -> bool {
        self.inner.is_ephemeral()
    }
    async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
        self.inner.get_lanes().await
    }
    async fn history_revision(&self) -> SessionResult<u64> {
        self.inner.history_revision().await
    }
    async fn append_entry_if_revision(
        &self,
        entry: ProvisionedEntry,
        lane: &str,
        expected: u64,
    ) -> SessionResult<Entry> {
        if matches!(&entry.kind, ProvisionedKind::Custom { custom_type, .. }
            if custom_type == self.marker)
        {
            return Err(SessionError::storage(format!(
                "controlled {} failure",
                self.marker
            )));
        }
        self.inner
            .append_entry_if_revision(entry, lane, expected)
            .await
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
        if matches!(&entry.kind, ProvisionedKind::Custom { custom_type, .. }
            if custom_type == self.marker)
        {
            return Err(SessionError::storage(format!(
                "controlled {} failure",
                self.marker
            )));
        }
        self.inner.append_entry(entry, lane).await
    }
    async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
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
    async fn drain(&self) {
        self.inner.drain().await
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_marker_storage_failures_cancel_run_without_retrying_effects() {
    for (marker, expected_effects) in [
        ("rpi.tool_program.intent.v1", 0),
        ("rpi.tool_program.finished.v1", 1),
    ] {
        let memory = AgentHarnessOptions::default().session;
        let session = Session::new(
            Arc::new(FaultStorage {
                inner: memory.storage(),
                marker,
            }),
            None,
        );
        let programs = Arc::new(AtomicUsize::new(0));
        let effects = Arc::new(AtomicUsize::new(0));
        let harness = build(session, programs.clone(), effects.clone(), false, false).await;
        let error = harness.prompt_text("run", vec![]).await.unwrap_err();
        assert!(error.to_string().contains(marker), "{error}");
        assert_eq!(programs.load(Ordering::SeqCst), 1);
        assert_eq!(effects.load(Ordering::SeqCst), expected_effects);
    }
}
