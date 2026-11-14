//! Real harness → SettlingEmitter → FrameRecordingEmitter regression cases.
//! Gates hold a live stream/tool invocation open without token-rate timing races.
use super::*;
use futures::future::BoxFuture;
use rpi_ai::providers::faux::DeferredPoll;
use rpi_ai::{AssistantMessageEvent, DeferredHandle, StopReason};
use rpi_harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
use rpi_harness::session::types::{AssistantFrameOp, Entry};
use rpi_harness::session::{validate_record_log, RecordLogSlice};
use rpi_harness::types::HarnessToolExecution;
use std::time::Duration;

#[path = "native_identity_duplicates.rs"]
mod native_identity_duplicates;

fn tool_result(call_id: &str) -> AgentMessage {
    rpi_ai::ToolResultMessage {
        role: Default::default(),
        tool_call_id: call_id.into(),
        tool_name: "read".into(),
        content: vec![rpi_ai::Content::text("same same same")],
        details: None,
        usage: None,
        added_tool_names: vec![],
        is_error: false,
        timestamp: 0,
    }
    .into()
}

struct GateEmitter {
    tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
    at_tool: bool,
}

impl AgentEmitter for GateEmitter {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        let block = if self.at_tool {
            // Start precedes preparation; the execute barrier journals only
            // approved effective args. End is after that ACK, before the result
            // MessageStart/commit, so it gates the recovery identity window.
            matches!(&event, AgentEvent::ToolExecutionEnd { .. })
        } else {
            matches!(
                &event,
                AgentEvent::MessageUpdate {
                    assistant_message_event: AssistantMessageEvent::TextDelta { .. },
                    ..
                }
            )
        };
        self.tx.send(event).unwrap();
        Box::pin(async move {
            if block {
                std::future::pending::<()>().await;
            }
        })
    }
    fn try_emit(&self, event: AgentEvent) {
        let _ = self.tx.send(event);
    }
}

async fn next_event(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) -> AgentEvent {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("harness reached deterministic gate")
        .expect("emitter still alive")
}

// This fixture drains all accepted messages at one boundary, so the crash
// occurs in generation only after every queued identity has been delivered.
async fn replayable_harness(
    session: Session,
    _replay: ToolReplay,
    _calls: Arc<std::sync::atomic::AtomicUsize>,
    allow_existing_session: bool,
    provider: Arc<FauxProvider>,
) -> AgentHarness {
    identity_harness(
        session,
        allow_existing_session,
        provider,
        HarnessToolExecution::default(),
    )
    .await
}

async fn identity_harness(
    session: Session,
    allow_existing_session: bool,
    provider: Arc<FauxProvider>,
    tool_execution: HarnessToolExecution,
) -> AgentHarness {
    let env = Arc::new(InMemoryExecutionEnv::new());
    let ctx = ExecutionToolContext::new(env.clone(), Some(env));
    AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        thinking_level: Default::default(),
        active_tool_names: vec!["read".into()],
        tools: vec![HarnessTool::new(create_read_tool(&ctx, None))],
        system_prompt: None,
        resources: Default::default(),
        stream_options: Default::default(),
        retry: RetryPolicy::default(),
        compaction: Default::default(),
        steering_mode: rpi_agent::QueueMode::All,
        follow_up_mode: Default::default(),
        tool_execution,
        drive: Default::default(),
        session,
        models: vec![provider as Arc<dyn Provider>],
        to_provider_messages: None,
        entry_projectors: Default::default(),
        agent_emitter: None,
        before_tool_call: None,
        after_tool_call: None,
        transform_context: None,
        entry_transforms: vec![],
        provider_hooks: None,
        allow_existing_session,
    })
    .await
    .unwrap()
}

async fn fixture(volatile: bool) -> (Session, Arc<dyn FileSystem>) {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryExecutionEnv::with_cwd("/".into()));
    let storage = JsonlSessionStorage::create(
        fs.clone(),
        "/native-ids.jsonl",
        JsonlV4Header::new("native-ids".into(), 1, "/".into(), None, None, None),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let session = Session::new(Arc::new(storage), None);
    (
        if volatile {
            session.with_volatile_progress()
        } else {
            session
        },
        fs,
    )
}

async fn reopen_session(session: Session, fs: Arc<dyn FileSystem>, volatile: bool) -> Session {
    if volatile {
        // Volatile progress is intentionally session-scoped, not cross-process.
        session
    } else {
        drop(session);
        let storage = JsonlSessionStorage::load(
            fs,
            "/native-ids.jsonl",
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        Session::new(Arc::new(storage), None)
    }
}

async fn crash_stream(volatile: bool, injected: bool) {
    let (session, fs) = fixture(volatile).await;
    let provider = FauxProvider::new(FauxScript::new().with_text("same same same"));
    let harness = replayable_harness(
        session.clone(),
        ToolReplay::Never,
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        false,
        provider,
    )
    .await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    harness
        .set_agent_emitter(Some(Arc::new(GateEmitter { tx, at_tool: false })))
        .await
        .unwrap();
    let mut queued = Vec::new();
    if injected {
        // Exercise the actual queue drain, not a manually wired mock binding.
        let assistant: AgentMessage =
            faux_assistant_message("same same same", StopReason::Stop).into();
        queued.push(harness.steer(assistant.clone()).await.unwrap().entry_id);
        queued.push(harness.steer(assistant).await.unwrap().entry_id);
        let result = tool_result("queued-call");
        queued.push(harness.steer(result).await.unwrap().entry_id);
    }
    let run = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.prompt_text("same same same", vec![]).await })
    };
    let mut current = None;
    let mut starts = Vec::new();
    let live_id = loop {
        match next_event(&mut rx).await {
            AgentEvent::MessageStart { message_id, .. } => {
                current = message_id;
                starts.push(
                    current
                        .clone()
                        .expect("real harness assigns Some before recorder"),
                );
            }
            AgentEvent::MessageUpdate {
                message_id,
                assistant_message_event: AssistantMessageEvent::TextDelta { .. },
                ..
            } => {
                assert_eq!(message_id, current, "generated slot owns its updates");
                break message_id.unwrap();
            }
            _ => {}
        }
    };
    assert_eq!(&starts[..queued.len()], queued.as_slice());
    assert!(
        !queued.contains(&live_id),
        "generated message cannot consume a queue reservation"
    );
    let open = session.find_open_operations("main", None).await.unwrap();
    let run_id = open[0].base.id.clone();
    // Wait for the recorder's durable checkpoint (not stream timing). In
    // volatile mode the update is already in the shared session cache.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let partial = rpi_harness::frame_progress::salvage_run_frames(&session, &run_id)
                .await
                .unwrap();
            if partial.iter().any(|message| {
                message.content.iter().any(
                    |block| matches!(block, rpi_ai::Content::Text(text) if !text.text.is_empty()),
                )
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("identified generated events must actually record frames");
    let frames = session.read_progress(&run_id).await.unwrap();
    if !volatile {
        assert!(
            frames.iter().any(|frame| frame.run_id == run_id
                && frame.stream_index == 0
                && frame.op == AssistantFrameOp::Append),
            "queue assistants must not shift generated stream ordinal"
        );
    } else {
        assert!(
            frames.is_empty(),
            "volatile payloads do not enter durable progress"
        );
    }
    run.abort();
    assert!(run.await.unwrap_err().is_cancelled());
    drop(harness);
    let session = reopen_session(session, fs, volatile).await;
    let recovered = reopen_harness(session.clone()).await;
    let Some(Entry::Message(entry)) = recovered.get_entry(&live_id).await.unwrap() else {
        panic!("recovery must persist the published ID, not mint a replacement");
    };
    let AgentMessage::Assistant(assistant) = entry.message else {
        panic!("assistant slot")
    };
    assert_eq!(assistant.stop_reason, StopReason::Error);
    assert!(assistant
        .content
        .iter()
        .any(|block| matches!(block, rpi_ai::Content::Text(text) if !text.text.is_empty())));
    let entries = session
        .view("main")
        .find_entries(&EntryQuery::default())
        .await
        .unwrap();
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.base().id == live_id)
            .count(),
        1
    );
    for id in queued {
        assert!(recovered.get_entry(&id).await.unwrap().is_some());
    }
    drop(recovered);
    let again = reopen_harness(session).await;
    assert!(
        again.get_entry(&live_id).await.unwrap().is_some(),
        "recovery is idempotent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_identified_stream_crash_cold_recovers_durable_id() {
    crash_stream(false, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_identified_stream_crash_recovers_volatile_id() {
    crash_stream(true, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_queued_assistant_and_result_do_not_shift_generated_recovery_slots() {
    for volatile in [false, true] {
        crash_stream(volatile, true).await;
    }
}

fn handle(provider: &FauxProvider) -> DeferredHandle {
    DeferredHandle {
        provider: provider.id().into(),
        model_id: provider.default_model().id.clone(),
        api: provider.default_model().api.as_str().into(),
        id: "polled-native".into(),
        expires_at: None,
        poll_after_ms: None,
        data: None,
    }
}
fn polled_message() -> rpi_ai::AssistantMessage {
    let mut message = faux_assistant_message("", StopReason::ToolUse);
    message.content = vec![
        rpi_ai::Content::text("before call"),
        rpi_ai::providers::faux::faux_thinking("before first call"),
        rpi_ai::Content::ToolCall(rpi_ai::ToolCall {
            kind: rpi_ai::ToolCallType,
            id: "polled-slot".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path":"a.txt"}),
            thought_signature: None,
            namespace: None,
            recorded: None,
        }),
        rpi_ai::Content::text("between calls"),
        rpi_ai::providers::faux::faux_thinking("before second call"),
        rpi_ai::Content::tool_call("polled-slot-2", "read", serde_json::json!({"path":"b.txt"})),
    ];
    message
}
async fn park(harness: &AgentHarness, handle: &DeferredHandle) {
    let mut parked = faux_assistant_message("", StopReason::Deferred);
    parked.deferred = Some(handle.clone());
    harness
        .session()
        .view("main")
        .append_message(parked.into())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_deferred_resume_tool_started_result_id_survives_recovery() {
    for volatile in [false, true] {
        let (session, fs) = fixture(volatile).await;
        let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
        let deferred = handle(&provider);
        provider.push_deferred_poll(DeferredPoll::Message(polled_message()));
        let harness = replayable_harness(
            session.clone(),
            ToolReplay::Never,
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            false,
            provider,
        )
        .await;
        park(&harness, &deferred).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        harness
            .set_agent_emitter(Some(Arc::new(GateEmitter { tx, at_tool: true })))
            .await
            .unwrap();
        let run = {
            let harness = harness.clone();
            tokio::spawn(async move { harness.resume(&deferred.id).await })
        };
        loop {
            if matches!(next_event(&mut rx).await, AgentEvent::ToolExecutionEnd { tool_call_id, .. } if tool_call_id == "polled-slot")
            {
                break;
            }
        }
        let records = session.find_records(&RecordQuery::default()).await.unwrap();
        let started = records
            .iter()
            .find_map(|record| match record {
                LaneRecord::ToolStarted(started) if started.tool_call_id == "polled-slot" => {
                    Some(started.clone())
                }
                _ => None,
            })
            .expect("valid poll resume must reserve the tool result before publication");
        assert_eq!(
            started.tool_index, 0,
            "ordinal counts only tool calls, not preceding text/thinking"
        );
        validate_native_session(&session).await;
        let Some(Entry::Message(entry)) = harness
            .get_entry(&started.assistant_entry_id)
            .await
            .unwrap()
        else {
            panic!("actual persisted poll entry")
        };
        assert!(
            matches!(entry.message, AgentMessage::Assistant(ref assistant) if matches!(&assistant.content[2], rpi_ai::Content::ToolCall(call) if call.id == "polled-slot"))
        );
        assert!(records.iter().any(|record| matches!(record, LaneRecord::StepAttempt(step)
            if step.run_id == started.run_id && step.result_entry_id == started.assistant_entry_id)));
        run.abort();
        assert!(run.await.unwrap_err().is_cancelled());
        validate_native_session(&session).await;
        drop(harness);
        let recovered = reopen_harness(reopen_session(session, fs, volatile).await).await;
        validate_native_session(recovered.session()).await;
        let Some(Entry::Message(entry)) =
            recovered.get_entry(&started.result_entry_id).await.unwrap()
        else {
            panic!("reserved result identity must survive tool crash")
        };
        assert!(
            matches!(entry.message, AgentMessage::ToolResult(ref result) if result.tool_call_id == "polled-slot")
        );
        assert!(
            recovered.resume_pending().await.unwrap().is_some(),
            "repaired deferred tool can continue"
        );
        validate_native_session(recovered.session()).await;
    }
}

// A deferred tool that intentionally terminates its batch, with no provider turn.
struct DeferredToolCallStop(rpi_ai::Tool);
#[async_trait::async_trait]
impl rpi_agent::AgentTool for DeferredToolCallStop {
    fn schema(&self) -> &rpi_ai::Tool {
        &self.0
    }
    fn label(&self) -> &str {
        "read"
    }
    async fn execute(
        &self,
        _: &str,
        _: serde_json::Value,
        _: tokio_util::sync::CancellationToken,
        _: Arc<dyn Fn(rpi_agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<rpi_agent::AgentToolResult, rpi_agent::AgentError> {
        let mut result = rpi_agent::AgentToolResult::text("deferred tool stopped");
        result.terminate = true;
        Ok(result)
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_deferred_tool_call_stop_resume_keeps_live_result_reservation() {
    let (harness, provider, _) =
        harness_with(FauxScript::new().with_text("must not call provider")).await;
    let tool = DeferredToolCallStop(rpi_ai::Tool {
        name: "read".into(),
        description: "stop".into(),
        parameters: rpi_ai::Schema::new(serde_json::json!({"type":"object"})),
        constrained_sampling: None,
    });
    harness
        .set_tools(vec![HarnessTool::new(Arc::new(tool))], None)
        .await
        .unwrap();
    let deferred = handle(&provider);
    provider.push_deferred_poll(DeferredPoll::Message(polled_message()));
    park(&harness, &deferred).await;
    let (collector, live) = CollectorEmitter::new();
    harness
        .set_agent_emitter(Some(Arc::new(collector)))
        .await
        .unwrap();
    harness
        .resume(&deferred.id)
        .await
        .expect("a terminating deferred tool can resume");
    let events = live.lock().unwrap().clone();
    assert_live_entry_ids(&harness, &events, false).await;
    assert_tool_call_ordinals(harness.session()).await;
    validate_native_session(harness.session()).await;
    assert!(events.iter().any(|event| matches!(event, AgentEvent::MessageEnd { message: AgentMessage::ToolResult(result), message_id: Some(_) } if result.tool_call_id == "polled-slot")));
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_missing_assistant_call_registration_still_fails_closed() {
    use rpi_harness::{
        frame_progress::FrameRecordingEmitter,
        settle::{SettleState, SettlingEmitter},
    };
    let (harness, _, _) = harness_with(FauxScript::new()).await;
    let (collector, events) = CollectorEmitter::new();
    let recorder = Arc::new(FrameRecordingEmitter::new(
        Arc::new(collector),
        harness.session().clone(),
        "main",
        "missing-call",
    ));
    let signal = tokio_util::sync::CancellationToken::new();
    let state = Arc::new(SettleState::new(
        "missing-call",
        false,
        0,
        Default::default(),
    ));
    let emitter = SettlingEmitter::new(
        recorder.clone(),
        recorder,
        state,
        harness.clone(),
        signal.clone(),
    );
    emitter
        .emit(AgentEvent::ToolExecutionStart {
            tool_call_id: "orphan".into(),
            tool_name: "read".into(),
            args: serde_json::json!({}),
        })
        .await;
    emitter
        .emit(AgentEvent::MessageStart {
            message: tool_result("orphan"),
            message_id: None,
        })
        .await;
    assert!(signal.is_cancelled());
    assert!(
        events.lock().unwrap().is_empty(),
        "no fresh fallback ID may escape"
    );
    assert!(!harness
        .session()
        .find_records(&RecordQuery::default())
        .await
        .unwrap()
        .iter()
        .any(|record| matches!(record, LaneRecord::ToolStarted(_))));
}

// Session has no validate(true) API in this checkout. Exercise its actual
// public native reducer with every entry/record in these bounded fixtures.
async fn validate_native_session(session: &Session) {
    let records = session
        .find_records(&RecordQuery {
            lane: Some("main".into()),
            limit: Some(1000),
            ..Default::default()
        })
        .await
        .unwrap();
    let entries = session
        .view("main")
        .find_entries(&EntryQuery {
            limit: Some(1000),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        records.len() < 1000 && entries.len() < 1000,
        "complete fixture slice"
    );
    validate_record_log(&RecordLogSlice {
        lane: "main".into(),
        open_operations: session.find_open_operations("main", None).await.unwrap(),
        records,
        entries,
    })
    .expect("full native record log satisfies reducer contract");
}

async fn assert_tool_call_ordinals(session: &Session) {
    let records = session
        .find_records(&RecordQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .unwrap();
    let calls = records
        .iter()
        .filter_map(|record| match record {
            LaneRecord::ToolStarted(started) => Some((
                started.tool_call_id.as_str(),
                started.tool_index,
                started.assistant_entry_id.as_str(),
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    assert_eq!((calls[0].0, calls[0].1), ("polled-slot", 0));
    assert_eq!((calls[1].0, calls[1].1), ("polled-slot-2", 1));
    assert_eq!(calls[0].2, calls[1].2, "same committed assistant");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_mixed_content_tool_ordinals_validate_normal_and_deferred() {
    for mode in [
        HarnessToolExecution::Parallel,
        HarnessToolExecution::Sequential,
    ] {
        for deferred_run in [false, true] {
            let (session, fs) = fixture(false).await;
            let script = FauxScript::new();
            script.set_responses(if deferred_run {
                vec![FauxStep::text("done")]
            } else {
                vec![FauxStep::message(polled_message()), FauxStep::text("done")]
            });
            let provider = FauxProvider::new(script);
            let deferred = handle(&provider);
            if deferred_run {
                provider.push_deferred_poll(DeferredPoll::Message(polled_message()));
            }
            let harness = identity_harness(session.clone(), false, provider, mode).await;
            let (collector, events) = CollectorEmitter::new();
            harness
                .set_agent_emitter(Some(Arc::new(collector)))
                .await
                .unwrap();
            if deferred_run {
                park(&harness, &deferred).await;
                tokio::time::timeout(Duration::from_secs(5), harness.resume(&deferred.id))
                    .await
                    .expect("bounded deferred completion")
                    .expect("deferred mixed-content calls complete");
            } else {
                let result = tokio::time::timeout(
                    Duration::from_secs(5),
                    harness.prompt_text("mixed calls", vec![]),
                )
                .await
                .expect("bounded normal completion")
                .expect("normal mixed-content calls complete");
                assert!(matches!(
                    result.outcome,
                    HarnessRunOutcome::Completed { .. }
                ));
            }
            let events = events.lock().unwrap().clone();
            assert_live_entry_ids(&harness, &events, false).await;
            assert_tool_call_ordinals(&session).await;
            validate_native_session(&session).await;
            drop(harness);
            let session = reopen_session(session, fs, false).await;
            validate_native_session(&session).await;
            let reopened = reopen_harness(session.clone()).await;
            assert_tool_call_ordinals(&session).await;
            validate_native_session(&session).await;
            assert_live_entry_ids(&reopened, &events, false).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_rejected_tool_calls_keep_error_result_ids_without_fake_tool_starts() {
    for mode in [
        HarnessToolExecution::Parallel,
        HarnessToolExecution::Sequential,
    ] {
        for (id, name, arguments, starts, executes) in [
            ("bad-name", "", serde_json::json!({}), false, false),
            ("blank-name", "  ", serde_json::json!({}), false, false),
            (
                "",
                "read",
                serde_json::json!({"path":"a.txt"}),
                false,
                false,
            ),
            (
                "  ",
                "read",
                serde_json::json!({"path":"a.txt"}),
                false,
                false,
            ),
            // Native argument coercion turns 7 into "7": execute is entered,
            // then the absent file fails. Journal its effective args, not 7.
            (
                "bad-args",
                "read",
                serde_json::json!({"path":7}),
                true,
                true,
            ),
            (
                "string-args",
                "read",
                serde_json::json!("not an object"),
                true,
                false,
            ),
        ] {
            let (session, fs) = fixture(false).await;
            let mut assistant = faux_assistant_message("", StopReason::ToolUse);
            // Empty IDs cannot correlate invocations. Each rejected message
            // still owns its native reservation, even in one malformed batch.
            let result_count = if id.is_empty() { 3 } else { 1 };
            assistant.content = vec![rpi_ai::Content::tool_call(id, name, arguments); result_count];
            let script = FauxScript::new();
            script.set_responses(vec![FauxStep::message(assistant), FauxStep::text("done")]);
            let harness =
                identity_harness(session.clone(), false, FauxProvider::new(script), mode).await;
            let (collector, events) = CollectorEmitter::new();
            harness
                .set_agent_emitter(Some(Arc::new(collector)))
                .await
                .unwrap();
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                harness.prompt_text("reject malformed call", vec![]),
            )
            .await
            .expect("bounded malformed-call completion")
            .expect("native rejection is a tool error result, not a fatal identity error");
            assert!(matches!(
                result.outcome,
                HarnessRunOutcome::Completed { .. }
            ));
            let events = events.lock().unwrap().clone();
            assert_live_entry_ids(&harness, &events, false).await;
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, AgentEvent::ToolExecutionStart { .. }))
                    .count(),
                usize::from(starts)
            );
            assert!(events.iter().any(|event| matches!(event,
                AgentEvent::ToolExecutionEnd { tool_call_id, is_error: true, .. }
                    if tool_call_id == id)));
            let result_ids = events
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::MessageEnd {
                        message: AgentMessage::ToolResult(result),
                        message_id: Some(entry_id),
                    } => {
                        assert_eq!(result.tool_call_id, id);
                        assert_eq!(result.tool_name, name);
                        assert!(result.is_error);
                        assert!(!result.content.is_empty());
                        Some(entry_id.clone())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(result_ids.len(), result_count);
            let records = session.find_records(&RecordQuery::default()).await.unwrap();
            let tool_starts = records
                .iter()
                .filter_map(|record| match record {
                    LaneRecord::ToolStarted(started) => Some(started),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                tool_starts.len(),
                usize::from(executes),
                "only actual execute entry journals intent: id={id:?} mode={mode:?}"
            );
            if executes {
                assert_eq!(
                    tool_starts[0].effective_args,
                    serde_json::json!({"path":"7"})
                );
                assert_eq!(tool_starts[0].result_entry_id, result_ids[0]);
            }
            validate_native_session(&session).await;
            drop(harness);
            let session = reopen_session(session, fs, false).await;
            validate_native_session(&session).await;
            let reopened = reopen_harness(session.clone()).await;
            assert_live_entry_ids(&reopened, &events, false).await;
            validate_native_session(&session).await;
        }
    }
}
