//! Journal at actual execute entry, after preparation and authorization.
use super::*;
use futures::future::BoxFuture;
use rpi_agent::{
    AgentEmitter, AgentError, AgentEvent, AgentTool, AgentToolResult, ToolResultPartial,
};
use rpi_ai::providers::faux::{faux_tool_call, FauxStep};
use rpi_harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
use rpi_harness::session::memory::SystemClock;
use rpi_harness::session::DefaultIdGenerator;
use rpi_harness::types::{HarnessTool, HarnessToolExecution};
use rpi_tools::{FileSystem, OsExecutionEnv};
use tokio_util::sync::CancellationToken;

struct ArgumentTool {
    schema: rpi_ai::types::Tool,
    seen: Mutex<Vec<(String, serde_json::Value)>>,
    preparations: AtomicUsize,
    pause: AtomicBool,
    entered: Notify,
    release: Notify,
}
#[async_trait::async_trait]
impl AgentTool for ArgumentTool {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "argument tool"
    }
    fn prepare_arguments(&self, args: serde_json::Value) -> Result<serde_json::Value, AgentError> {
        self.preparations.fetch_add(1, Ordering::SeqCst);
        Ok(match args.get("legacy") {
            Some(value) => serde_json::json!({"target": value}),
            None => args,
        })
    }
    async fn execute(
        &self,
        id: &str,
        args: serde_json::Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        self.seen.lock().unwrap().push((id.into(), args));
        if self.pause.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(AgentToolResult::text("executed"))
    }
}

struct PauseAtEnd(Arc<Notify>);
impl AgentEmitter for PauseAtEnd {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        let entered = self.0.clone();
        Box::pin(async move {
            if matches!(event, AgentEvent::ToolExecutionEnd { .. }) {
                entered.notify_one();
                std::future::pending::<()>().await;
            }
        })
    }
    fn try_emit(&self, _: AgentEvent) {}
}

async fn setup(
    script: FauxScript,
    mode: HarnessToolExecution,
    before: Option<rpi_agent::BeforeToolCall>,
    emitter: Option<Arc<dyn AgentEmitter>>,
) -> (AgentHarness, Arc<ProbedStorage>, Arc<ArgumentTool>) {
    setup_on_storage(
        script,
        mode,
        before,
        emitter,
        AgentHarnessOptions::default().session.storage(),
    )
    .await
}

async fn setup_on_storage(
    script: FauxScript,
    mode: HarnessToolExecution,
    before: Option<rpi_agent::BeforeToolCall>,
    emitter: Option<Arc<dyn AgentEmitter>>,
    inner: Arc<dyn SessionStorage>,
) -> (AgentHarness, Arc<ProbedStorage>, Arc<ArgumentTool>) {
    let defaults = AgentHarnessOptions::default();
    let storage = Arc::new(ProbedStorage::new(inner));
    storage.pause.store(false, Ordering::SeqCst);
    let tool = Arc::new(ArgumentTool {
        schema: rpi_ai::types::Tool {
            name: "argument".into(),
            description: "argument journal probe".into(),
            parameters: rpi_ai::types::Schema::new(serde_json::json!({
                "type":"object", "properties":{"target":{"type":"string"}}, "required":["target"]
            })),
            constrained_sampling: None,
        },
        seen: Mutex::new(Vec::new()),
        preparations: AtomicUsize::new(0),
        pause: AtomicBool::new(false),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let provider = FauxProvider::new(script);
    let harness = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        session: Session::new(storage.clone(), None),
        tools: vec![
            HarnessTool::new(tool.clone()).with_replay(rpi_harness::types::ToolReplay::Safe)
        ],
        tool_execution: mode,
        before_tool_call: before,
        agent_emitter: emitter,
        ..defaults
    })
    .await
    .unwrap();
    (harness, storage, tool)
}
fn script() -> FauxScript {
    FauxScript::new()
        .with_tool_call("argument", serde_json::json!({"legacy":"prepared"}))
        .with_text("done")
}
async fn journal(storage: &ProbedStorage) -> Vec<ToolStartedRecord> {
    records(storage, "tool_started")
        .await
        .into_iter()
        .map(|record| match record {
            LaneRecord::ToolStarted(record) => record,
            _ => unreachable!(),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn persistence_failure_before_execution_keeps_side_effects_out_and_operation_open() {
    for mode in [
        HarnessToolExecution::Sequential,
        HarnessToolExecution::Parallel,
    ] {
        for assistant_failure in [true, false] {
            let (harness, storage, tool) = setup(script(), mode, None, None).await;
            if assistant_failure {
                storage.fail_assistant.store(true, Ordering::SeqCst);
            } else {
                storage.fail_start.store(true, Ordering::SeqCst);
            }
            let ended = Arc::new(AtomicUsize::new(0));
            let count = ended.clone();
            let _listener = harness
                .events()
                .on::<rpi_harness::events::RunEndEvent, _>(move |_| {
                    count.fetch_add(1, Ordering::SeqCst);
                });
            let result = harness.prompt_text("execute", vec![]).await;
            assert!(
                tool.seen.lock().unwrap().is_empty(),
                "unacknowledged execution must not reach the real tool"
            );
            assert!(
                result.is_err(),
                "persistence failure must propagate: {result:?}"
            );
            assert!(records(&storage, "operation_finished").await.is_empty());
            assert_eq!(ended.load(Ordering::SeqCst), 0);
            assert_eq!(
                storage
                    .find_open_operations("main", None)
                    .await
                    .unwrap()
                    .len(),
                1
            );
            drop(harness);
            drop(
                AgentHarness::create(AgentHarnessOptions {
                    session: Session::new(storage.clone(), None),
                    allow_existing_session: true,
                    tools: vec![HarnessTool::new(tool.clone())
                        .with_replay(rpi_harness::types::ToolReplay::Safe)],
                    ..Default::default()
                })
                .await
                .unwrap(),
            );
            assert!(
                tool.seen.lock().unwrap().is_empty(),
                "a call with no durable start cannot be replayed"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepared_parameters_are_persisted_and_replayed_without_preparing_again() {
    for mode in [
        HarnessToolExecution::Sequential,
        HarnessToolExecution::Parallel,
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(tmp.path().into()));
        let path = tmp
            .path()
            .join("prepared.jsonl")
            .to_string_lossy()
            .into_owned();
        let inner = JsonlSessionStorage::create(
            fs.clone(),
            &path,
            JsonlV4Header::new(
                "prepared-checkpoint".into(),
                1,
                "/".into(),
                None,
                None,
                None,
            ),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let entered = Arc::new(Notify::new());
        let mut assistant = faux_assistant_message("text before the call", StopReason::ToolUse);
        assistant.content.push(faux_tool_call(
            "argument",
            serde_json::json!({"legacy":"prepared"}),
        ));
        let script = FauxScript::new();
        script.set_responses(vec![FauxStep::message(assistant), FauxStep::text("done")]);
        let (harness, storage, tool) = setup_on_storage(
            script,
            mode,
            None,
            Some(Arc::new(PauseAtEnd(entered.clone()))),
            Arc::new(inner),
        )
        .await;
        let active = harness.clone();
        let run = tokio::spawn(async move { active.prompt_text("execute", vec![]).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        let starts = journal(&storage).await;
        assert_eq!(starts.len(), 1);
        let args = tool.seen.lock().unwrap()[0].1.clone();
        assert_eq!(args, serde_json::json!({"target":"prepared"}));
        assert_eq!(
            starts[0].effective_args, args,
            "journal must record execute arguments, not raw provider arguments"
        );
        run.abort();
        let _ = run.await;
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
                allow_existing_session: true,
                tools: vec![HarnessTool::new(tool.clone())
                    .with_replay(rpi_harness::types::ToolReplay::Safe)],
                ..Default::default()
            })
            .await
            .unwrap();
            let result = restored
                .session()
                .storage()
                .get_entry(&starts[0].result_entry_id)
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(result, Entry::Message(entry) if matches!(&entry.message,
                AgentMessage::ToolResult(result) if !result.is_error))
            );
        }
        assert_eq!(
            tool.preparations.load(Ordering::SeqCst),
            1,
            "recovery must execute the journal arguments without preparing again"
        );
        let seen = tool.seen.lock().unwrap();
        assert_eq!(
            seen.len(),
            2,
            "one original execution plus one Safe recovery, no repeat on second reopen"
        );
        assert_eq!(seen[0], seen[1]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authorized_replacement_is_journaled_without_revalidation() {
    for mode in [
        HarnessToolExecution::Sequential,
        HarnessToolExecution::Parallel,
    ] {
        let before: rpi_agent::BeforeToolCall = Arc::new(|ctx, _| {
            assert_eq!(*ctx.args, serde_json::json!({"target":"prepared"}));
            Box::pin(async {
                Some(rpi_agent::BeforeToolCallResult {
                    args: Some(serde_json::json!({"target":42})),
                    ..Default::default()
                })
            })
        });
        let (harness, storage, tool) = setup(script(), mode, Some(before), None).await;
        harness.prompt_text("execute", vec![]).await.unwrap();
        let seen = tool.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].1, serde_json::json!({"target":42}));
        assert_eq!(journal(&storage).await[0].effective_args, seen[0].1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn calls_that_never_execute_have_no_durable_start() {
    for mode in [
        HarnessToolExecution::Sequential,
        HarnessToolExecution::Parallel,
    ] {
        for kind in ["blocked", "invalid", "recorded"] {
            let mut call = faux_tool_call("argument", serde_json::json!({"bad":true}));
            if let rpi_ai::types::Content::ToolCall(call) = &mut call {
                if kind != "invalid" {
                    call.arguments = serde_json::json!({"legacy":"prepared"});
                }
                if kind == "recorded" {
                    call.recorded = Some(serde_json::json!({
                        "version":1,"isError":false,"result":{"content":[{"type":"text","text":"upstream"}]}
                    }));
                }
            }
            let mut assistant = faux_assistant_message("", StopReason::ToolUse);
            assistant.content = vec![call];
            let script = FauxScript::new();
            script.set_responses(vec![FauxStep::message(assistant), FauxStep::text("done")]);
            let before = if kind == "blocked" {
                Some(Arc::new(
                    |_: rpi_agent::BeforeToolCallContext<'_>, _: CancellationToken| -> BoxFuture<'static, Option<rpi_agent::BeforeToolCallResult>> {
                        Box::pin(async {
                            Some(rpi_agent::BeforeToolCallResult {
                                block: true,
                                ..Default::default()
                            })
                        })
                    },
                ) as rpi_agent::BeforeToolCall)
            } else {
                None
            };
            let (harness, storage, tool) = setup(script, mode, before, None).await;
            harness.prompt_text("execute", vec![]).await.unwrap();
            assert!(tool.seen.lock().unwrap().is_empty());
            assert!(
                journal(&storage).await.is_empty(),
                "{kind} is not a real execution"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_while_start_is_pending_prevents_real_execute() {
    let (harness, storage, tool) =
        setup(script(), HarnessToolExecution::Parallel, None, None).await;
    storage.pause_start.store(true, Ordering::SeqCst);
    let active = harness.clone();
    let run = tokio::spawn(async move { active.prompt_text("execute", vec![]).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        storage.start_entered.notified(),
    )
    .await
    .unwrap();
    harness.abort().await.unwrap();
    storage.start_release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(tool.seen.lock().unwrap().is_empty());
    assert!(matches!(result.outcome, HarnessRunOutcome::Aborted { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_start_does_not_detach_an_already_executing_parallel_peer() {
    let mut assistant = faux_assistant_message("", StopReason::ToolUse);
    assistant.content = vec![
        faux_tool_call("argument", serde_json::json!({"legacy":"one"})),
        faux_tool_call("argument", serde_json::json!({"legacy":"two"})),
    ];
    let script = FauxScript::new();
    script.set_responses(vec![FauxStep::message(assistant), FauxStep::text("unused")]);
    let (harness, storage, tool) = setup(script, HarnessToolExecution::Parallel, None, None).await;
    storage.pause_start.store(true, Ordering::SeqCst);
    storage.fail_start.store(true, Ordering::SeqCst);
    tool.pause.store(true, Ordering::SeqCst);
    let active = harness.clone();
    let mut run = tokio::spawn(async move { active.prompt_text("execute", vec![]).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        storage.start_entered.notified(),
    )
    .await
    .unwrap();
    let peer =
        tokio::time::timeout(std::time::Duration::from_secs(2), tool.entered.notified()).await;
    storage.start_release.notify_one();
    assert!(
        peer.is_ok(),
        "the independent parallel peer should reach execute while the first marker waits"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), &mut run)
            .await
            .is_err(),
        "the run must wait for the already executing peer to settle"
    );
    tool.release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());
    assert_eq!(
        tool.seen.lock().unwrap().len(),
        1,
        "only the peer with an acknowledged start executed"
    );
    assert!(records(&storage, "operation_finished").await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn execution_ordinals_count_calls_not_text_blocks() {
    for mode in [
        HarnessToolExecution::Sequential,
        HarnessToolExecution::Parallel,
    ] {
        let mut assistant = faux_assistant_message("", StopReason::ToolUse);
        assistant.content = vec![
            rpi_ai::types::Content::text("preamble"),
            faux_tool_call("argument", serde_json::json!({"legacy":"one"})),
            rpi_ai::types::Content::text("between calls"),
            faux_tool_call("argument", serde_json::json!({"legacy":"two"})),
        ];
        let script = FauxScript::new();
        script.set_responses(vec![FauxStep::message(assistant), FauxStep::text("done")]);
        let (harness, storage, tool) = setup(script, mode, None, None).await;
        let result = harness.prompt_text("execute mixed content", vec![]).await;
        assert!(
            result.is_ok(),
            "text blocks must not corrupt execution ordinals: {result:?}"
        );
        assert_eq!(tool.seen.lock().unwrap().len(), 2);
        let mut starts = journal(&storage).await;
        starts.sort_by_key(|start| start.tool_index);
        assert_eq!(
            starts
                .iter()
                .map(|start| start.tool_index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        for (start, value) in starts.iter().zip(["one", "two"]) {
            assert_eq!(start.effective_args, serde_json::json!({"target":value}));
            assert!(storage
                .get_entry(&start.result_entry_id)
                .await
                .unwrap()
                .is_some());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_new_assistant_cannot_reuse_a_previously_blocked_call_identity() {
    for mode in [
        HarnessToolExecution::Sequential,
        HarnessToolExecution::Parallel,
    ] {
        let storage_slot: Arc<Mutex<Option<Arc<ProbedStorage>>>> = Arc::new(Mutex::new(None));
        let slot = storage_slot.clone();
        let before: rpi_agent::BeforeToolCall = Arc::new(move |_, _| {
            let first = slot.lock().unwrap().take();
            Box::pin(async move {
                first.map(|storage| {
                    storage.fail_assistant.store(true, Ordering::SeqCst);
                    rpi_agent::BeforeToolCallResult {
                        block: true,
                        ..Default::default()
                    }
                })
            })
        });
        let mut first = faux_assistant_message("", StopReason::ToolUse);
        first.content = vec![faux_tool_call(
            "argument",
            serde_json::json!({"legacy":"first"}),
        )];
        let mut second = first.clone();
        if let rpi_ai::types::Content::ToolCall(call) = &mut second.content[0] {
            call.arguments = serde_json::json!({"legacy":"second"});
        }
        let script = FauxScript::new();
        script.set_responses(vec![
            FauxStep::message(first),
            FauxStep::message(second),
            FauxStep::text("unused"),
        ]);
        let (harness, storage, tool) = setup(script, mode, Some(before), None).await;
        *storage_slot.lock().unwrap() = Some(storage.clone());
        let result = harness.prompt_text("execute", vec![]).await;
        assert!(
            tool.seen.lock().unwrap().is_empty(),
            "the uncommitted assistant cannot borrow an older call's identity"
        );
        assert!(result.is_err());
        assert!(journal(&storage).await.is_empty());
        assert!(records(&storage, "operation_finished").await.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_blocked_reused_call_cannot_borrow_a_previous_result_reservation() {
    for mode in [
        HarnessToolExecution::Sequential,
        HarnessToolExecution::Parallel,
    ] {
        let turns = Arc::new(AtomicUsize::new(0));
        let before: rpi_agent::BeforeToolCall = Arc::new(move |_, _| {
            let block = turns.fetch_add(1, Ordering::SeqCst) != 0;
            Box::pin(async move {
                block.then_some(rpi_agent::BeforeToolCallResult {
                    block: true,
                    ..Default::default()
                })
            })
        });
        let mut first = faux_assistant_message("", StopReason::ToolUse);
        first.content = vec![faux_tool_call(
            "argument",
            serde_json::json!({"legacy":"first"}),
        )];
        let mut second = first.clone();
        if let rpi_ai::types::Content::ToolCall(call) = &mut second.content[0] {
            call.arguments = serde_json::json!({"legacy":"blocked"});
        }
        let script = FauxScript::new();
        script.set_responses(vec![
            FauxStep::message(first),
            FauxStep::message(second),
            FauxStep::text("done"),
        ]);
        let (harness, storage, tool) = setup(script, mode, Some(before), None).await;
        let result = harness.prompt_text("execute", vec![]).await;
        assert!(
            result.is_ok(),
            "a later blocked result must not reuse the executed call's entry ID: {result:?}"
        );
        assert_eq!(tool.seen.lock().unwrap().len(), 1);
        let starts = journal(&storage).await;
        assert_eq!(starts.len(), 1);
        let results: Vec<_> = storage
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Message(entry) if matches!(&entry.message, AgentMessage::ToolResult(_)) => {
                    Some(entry)
                }
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].base.id, starts[0].result_entry_id);
        assert_ne!(results[0].base.id, results[1].base.id);
        assert!(matches!(&results[1].message, AgentMessage::ToolResult(result) if result.is_error));
    }
}
