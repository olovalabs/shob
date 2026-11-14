//! Share the fault probe with the live recovery tests, not its production API.
#[path = "settle_prefix_order.rs"]
mod settle_prefix_order;

use super::*;
use rpi_agent::{AgentError, AgentTool, AgentToolResult, ToolResultPartial};
use rpi_harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
use rpi_harness::session::{DefaultIdGenerator, SystemClock};
use rpi_harness::types::HarnessTool;
use rpi_tools::{FileSystem, OsExecutionEnv};
use tokio_util::sync::CancellationToken;

struct CountingTool {
    schema: rpi_ai::types::Tool,
    calls: AtomicUsize,
    context: Mutex<Option<AgentHarness>>,
    terminate: bool,
}

#[async_trait::async_trait]
impl AgentTool for CountingTool {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "count"
    }
    async fn execute(
        &self,
        _call_id: &str,
        _args: serde_json::Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let harness = self.context.lock().unwrap().take();
        if let Some(harness) = harness {
            harness
                .append_message(rpi_ai::types::UserMessage::new("accepted context", 0).into())
                .await
                .unwrap();
        }
        let mut result = AgentToolResult::text("the actual tool result");
        result.terminate = self.terminate;
        Ok(result)
    }
}

async fn tool_fixture(
    inner: Arc<dyn SessionStorage>,
    failures: usize,
    context: bool,
) -> (AgentHarness, Arc<ProbedStorage>, Arc<CountingTool>) {
    let (harness, storage, tool, _) = scripted_tool_fixture(
        inner,
        failures,
        context,
        true,
        FauxScript::new().with_tool_call("count", serde_json::json!({})),
    )
    .await;
    (harness, storage, tool)
}

async fn scripted_tool_fixture(
    inner: Arc<dyn SessionStorage>,
    failures: usize,
    context: bool,
    terminate: bool,
    script: FauxScript,
) -> (
    AgentHarness,
    Arc<ProbedStorage>,
    Arc<CountingTool>,
    Arc<FauxProvider>,
) {
    let storage = Arc::new(ProbedStorage::new(inner));
    storage.pause.store(false, Ordering::SeqCst);
    *storage.fail_results.lock().unwrap() = failures;
    let tool = Arc::new(CountingTool {
        schema: rpi_ai::types::Tool {
            name: "count".into(),
            description: "count side effects".into(),
            parameters: rpi_ai::types::Schema::new(serde_json::json!({"type":"object"})),
            constrained_sampling: None,
        },
        calls: AtomicUsize::new(0),
        context: Mutex::new(None),
        terminate,
    });
    let provider = FauxProvider::new(script);
    let harness = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider.clone()],
        tools: vec![HarnessTool::new(tool.clone())],
        session: Session::new(storage.clone(), None),
        ..Default::default()
    })
    .await
    .unwrap();
    if context {
        *tool.context.lock().unwrap() = Some(harness.clone());
    }
    (harness, storage, tool, provider)
}

async fn started(storage: &dyn SessionStorage) -> ToolStartedRecord {
    let records = storage
        .find_records(&RecordQuery {
            record_type: Some("tool_started"),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    let LaneRecord::ToolStarted(record) = &records[0] else {
        unreachable!()
    };
    record.clone()
}

async fn assert_valid(storage: &dyn SessionStorage) {
    rpi_harness::session::validate_record_log(&rpi_harness::session::RecordLogSlice {
        lane: "main".into(),
        open_operations: storage.find_open_operations("main", None).await.unwrap(),
        records: storage.find_records(&RecordQuery::default()).await.unwrap(),
        entries: storage.find_entries(&EntryQuery::default()).await.unwrap(),
    })
    .unwrap();
}

async fn assert_bound_result(storage: &dyn SessionStorage) -> MessageEntry {
    let record = started(storage).await;
    let entries = storage
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .unwrap();
    let assistant = entries
        .iter()
        .position(|entry| entry.id() == record.assistant_entry_id)
        .unwrap();
    let result = entries
        .iter()
        .position(|entry| entry.id() == record.result_entry_id)
        .expect("the reserved tool result must be committed");
    assert!(result > assistant);
    assert!(
        entries[assistant + 1..result].iter().all(|entry| matches!(
            entry,
            Entry::Custom(_)
                | Entry::Message(MessageEntry {
                    message: AgentMessage::ToolResult(_),
                    ..
                })
        )),
        "recovery must fill the durable tool batch before a later assistant"
    );
    let results: Vec<_> = entries
        .into_iter()
        .filter_map(|entry| match entry {
            Entry::Message(message) if matches!(message.message, AgentMessage::ToolResult(_)) => {
                Some(message)
            }
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1, "exactly one tool result");
    assert_eq!(results[0].base.id, record.result_entry_id);
    let AgentMessage::ToolResult(result) = &results[0].message else {
        unreachable!()
    };
    assert_eq!(result.tool_call_id, record.tool_call_id);
    assert_eq!(result.tool_name, record.tool_name);
    assert_valid(storage).await;
    results.into_iter().next().unwrap()
}

async fn fallback(context: bool) {
    let (harness, storage, tool) =
        tool_fixture(AgentHarnessOptions::default().session.storage(), 1, context).await;
    let result = harness.prompt_text("run tool", vec![]).await.unwrap();
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    let reserved = started(storage.as_ref()).await.result_entry_id;
    assert_eq!(
        *storage.result_attempts.lock().unwrap(),
        vec![reserved.clone(), reserved],
        "settle and fallback must try the same reserved entry ID"
    );
    let entry = assert_bound_result(storage.as_ref()).await;
    let AgentMessage::ToolResult(message) = entry.message else {
        unreachable!()
    };
    assert!(!message.is_error);
    assert_eq!(
        message.content[0],
        rpi_ai::types::Content::text("the actual tool result")
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    if context {
        let entries = storage.find_entries(&EntryQuery::default()).await.unwrap();
        let context_entry = entries.iter().find(|entry| matches!(entry,
            Entry::Message(message) if matches!(&message.message,
                AgentMessage::User(user) if user.content == rpi_ai::types::UserContent::Text("accepted context".into()))
        )).expect("accepted context was committed");
        assert!(context_entry.seq() > entry.base.seq);
    }
    assert!(records(&storage, "operation_finished").await.len() == 1);
    assert!(storage
        .read_progress(&result.run_id)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminated_turn_fallback_reuses_the_tool_started_result_id() {
    fallback(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_boundary_fallback_reuses_the_tool_started_result_id() {
    fallback(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_settle_and_fallback_keep_one_reserved_result_through_jsonl_reopen() {
    let tmp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(tmp.path().into()));
    let path = tmp
        .path()
        .join("fallback.jsonl")
        .to_string_lossy()
        .into_owned();
    let inner = JsonlSessionStorage::create(
        fs.clone(),
        &path,
        JsonlV4Header::new("fallback".into(), 1, "/".into(), None, None, None),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let (harness, storage, tool) = tool_fixture(Arc::new(inner), 2, false).await;
    assert!(harness.prompt_text("run tool", vec![]).await.is_err());
    assert!(records(&storage, "operation_finished").await.is_empty());
    assert_eq!(
        storage
            .find_open_operations("main", None)
            .await
            .unwrap()
            .len(),
        1
    );
    let record = started(storage.as_ref()).await;
    assert_eq!(
        *storage.result_attempts.lock().unwrap(),
        vec![
            record.result_entry_id.clone(),
            record.result_entry_id.clone()
        ]
    );
    assert!(storage
        .get_entry(&record.result_entry_id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    drop(harness);
    drop(storage);
    for _ in 0..2 {
        let reopened = JsonlSessionStorage::load(
            fs.clone(),
            &path,
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let restored = AgentHarness::create(AgentHarnessOptions {
            session: Session::new(Arc::new(reopened), None),
            tools: vec![HarnessTool::new(tool.clone())],
            allow_existing_session: true,
            ..Default::default()
        })
        .await
        .unwrap();
        let entry = assert_bound_result(restored.session().storage().as_ref()).await;
        assert_eq!(entry.base.id, record.result_entry_id);
        assert_eq!(
            tool.calls.load(Ordering::SeqCst),
            1,
            "Never tools cannot be rerun"
        );
    }
}
