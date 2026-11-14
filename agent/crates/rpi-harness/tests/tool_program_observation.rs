//! Native observation through the real harness wrappers/checkpoint barrier.
//! The mock is host-owned; no tool calls prepared() or fabricates child messages.
use futures::future::BoxFuture;
use rpi_agent::{
    tool_program::{current, nested_parent, ToolProgram},
    tool_program_observation::{
        current_observation, ToolProgramChild, ToolProgramObservation, ToolProgramObserver,
    },
    AfterToolCallResult, AgentEmitter, AgentError, AgentEvent, AgentMessage, AgentTool,
    AgentToolResult, BeforeToolCallResult, ToolExecutionMode, ToolResultPartial,
};
use rpi_ai::{
    providers::faux::{FauxProvider, FauxScript},
    types::{Content, Tool, Usage},
};
use rpi_harness::{
    agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome},
    session::{types::*, Session},
    types::{AgentHarnessOptions, HarnessTool},
};
use serde_json::{json, Value};
use std::{
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

type Update = Arc<dyn Fn(ToolResultPartial) + Send + Sync>;
type Body = Arc<
    dyn Fn(
            String,
            Value,
            CancellationToken,
            Update,
        ) -> BoxFuture<'static, Result<AgentToolResult, AgentError>>
        + Send
        + Sync,
>;
type Prepare = fn(Value) -> Result<Value, AgentError>;

struct NativeTool {
    schema: Tool,
    body: Body,
    prepare: Prepare,
}
#[async_trait::async_trait]
impl AgentTool for NativeTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        &self.schema.name
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        if matches!(self.schema.name.as_str(), "program" | "wrapper") {
            ToolExecutionMode::Sequential
        } else {
            ToolExecutionMode::Parallel
        }
    }
    fn prepare_arguments(&self, args: Value) -> Result<Value, AgentError> {
        (self.prepare)(args)
    }
    async fn execute(
        &self,
        id: &str,
        args: Value,
        signal: CancellationToken,
        update: Update,
    ) -> Result<AgentToolResult, AgentError> {
        // Deliberately no simulated barrier: ToolExecutionBarrier owns it.
        (self.body)(id.into(), args, signal, update).await
    }
}
fn tool(name: &str, body: Body) -> HarnessTool {
    native(name, json!({"type":"object"}), Ok, body)
}
fn native(name: &str, parameters: Value, prepare: Prepare, body: Body) -> HarnessTool {
    HarnessTool::new(Arc::new(NativeTool {
        schema: serde_json::from_value(json!({
            "name":name,"description":"observation integration fixture","parameters":parameters
        }))
        .unwrap(),
        body,
        prepare,
    }))
}
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("fixture deadlocked")
}
async fn start(program: &Arc<ToolProgram>, name: &str, arguments: Value) -> Value {
    program
        .handle(json!({"op":"start","name":name,"arguments":arguments}))
        .await
        .unwrap()["requestId"]
        .clone()
}
async fn poll(program: &Arc<ToolProgram>, id: &Value) -> Value {
    bounded(async {
        loop {
            let reply = program
                .handle(json!({"op":"poll","requestId":id}))
                .await
                .unwrap();
            if reply["status"] != "running" {
                return reply;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
}
fn signed_partial() -> AgentToolResult {
    AgentToolResult {
        content: vec![rpi_agent::TextContentOrImage::Text(
            serde_json::from_value(json!({
                "type":"text","text":"unmodified partial","textSignature":"native-signature"
            }))
            .unwrap(),
        )],
        details: json!({"private":{"values":[1,2,3],"nested":{"original":true}}}),
        ..Default::default()
    }
}
fn patched_usage() -> Usage {
    let mut usage = Usage::zero();
    usage.input = 7;
    usage.output = 4;
    usage.total_tokens = 11;
    usage
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreparedFailure {
    None,
    Error,
    Panic,
}
struct Log {
    session: Session,
    events: Mutex<Vec<Value>>,
    children: Mutex<Vec<(ToolProgramChild, Value)>>,
    failure: PreparedFailure,
    close_entered: Notify,
    close_gate: Option<Arc<Notify>>,
}
impl Log {
    fn new(
        session: Session,
        failure: PreparedFailure,
        close_gate: Option<Arc<Notify>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            session,
            events: Mutex::new(vec![]),
            children: Mutex::new(vec![]),
            failure,
            close_entered: Notify::new(),
            close_gate,
        })
    }
    fn event(&self, event: Value) {
        self.events.lock().unwrap().push(event);
    }
    fn snapshot(&self) -> Vec<Value> {
        self.events.lock().unwrap().clone()
    }
    fn phases(&self) -> Vec<String> {
        self.snapshot()
            .iter()
            .map(|e| e["phase"].as_str().unwrap().into())
            .collect()
    }
}
struct Observer(Arc<Log>);
impl ToolProgramObserver for Observer {
    fn begin<'a>(
        &'a self,
        child: ToolProgramChild,
        args: &'a Value,
        _: CancellationToken,
    ) -> BoxFuture<'a, Result<Arc<dyn ToolProgramObservation>, String>> {
        Box::pin(async move {
            self.0
                .event(json!({"phase":"begin","requestId":child.request_id,"arguments":args}));
            self.0
                .children
                .lock()
                .unwrap()
                .push((child.clone(), args.clone()));
            let observation: Arc<dyn ToolProgramObservation> = Arc::new(Observation {
                id: format!("host-entry-{}", child.ordinal),
                child,
                log: self.0.clone(),
            });
            Ok(observation)
        })
    }
}
struct Observation {
    id: String,
    child: ToolProgramChild,
    log: Arc<Log>,
}
impl ToolProgramObservation for Observation {
    fn id(&self) -> &str {
        &self.id
    }
    fn update(&self, partial: Arc<AgentToolResult>) -> Result<(), String> {
        self.log.event(json!({"phase":"update","partial":partial}));
        Ok(())
    }
    fn prepared<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            // Inspect ACKed native state at the observation boundary, not after execute.
            let entries = self
                .log
                .session
                .find_entries(&EntryQuery::default())
                .await
                .unwrap();
            let records = self
                .log
                .session
                .find_records(&RecordQuery::default())
                .await
                .unwrap();
            let root = records
                .iter()
                .find_map(|r| match r {
                    LaneRecord::ToolStarted(r) if r.tool_call_id == self.child.root_id => Some(r),
                    _ => None,
                })
                .expect("root checkpoint ACK must precede prepared");
            assert_eq!(root.replay, ToolReplay::Never);
            assert_eq!(root.tool_name, self.child.root_name);
            assert!(
                entries.iter().any(|entry| matches!(entry,
                Entry::Message(message) if message.base.id == root.assistant_entry_id
                    && matches!(&message.message, AgentMessage::Assistant(assistant)
                        if assistant.content.iter().any(|c| matches!(c,
                            Content::ToolCall(call) if call.id == root.tool_call_id
                                && call.name == root.tool_name))))),
                "the root owner must be an actual persisted assistant call"
            );
            let intent = entries
                .iter()
                .find_map(|entry| match entry {
                    Entry::Custom(entry)
                        if entry.custom_type == "rpi.tool_program.intent.v1"
                            && entry
                                .data
                                .as_ref()
                                .is_some_and(|d| d["requestId"] == self.child.request_id) =>
                    {
                        entry.data.as_ref()
                    }
                    _ => None,
                })
                .expect("native child intent ACK must precede prepared");
            assert_eq!(intent["rootToolCallId"], self.child.root_id);
            assert_eq!(intent["parentToolCallId"], self.child.parent_id);
            assert_eq!(intent["runId"], root.run_id);
            assert_eq!(intent["name"], self.child.name);
            assert_eq!(intent["arguments"], *args);
            assert_eq!(intent["replay"], "never");
            assert!(self.child.request_id.starts_with("tp-"));
            assert_ne!(self.id, self.child.request_id);
            self.log.event(json!({"phase":"prepared","arguments":args}));
            match self.log.failure {
                PreparedFailure::None => Ok(()),
                PreparedFailure::Error => Err("controlled prepared sink failure".into()),
                PreparedFailure::Panic => panic!("controlled prepared sink panic"),
            }
        })
    }
    fn finish<'a>(
        &'a self,
        result: &'a AgentToolResult,
        is_error: bool,
        cancelled: bool,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.log.event(
                json!({"phase":"finish","result":result,"isError":is_error,"cancelled":cancelled}),
            );
            Ok(())
        })
    }
    fn close<'a>(&'a self, reason: Option<&'a str>) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.log.event(json!({"phase":"closing","reason":reason}));
            self.log.close_entered.notify_one();
            if let Some(gate) = &self.log.close_gate {
                gate.notified().await;
            }
            self.log.event(json!({"phase":"closed"}));
            Ok(())
        })
    }
}
struct Emitter {
    observer: Option<Arc<dyn ToolProgramObserver>>,
    events: Arc<Mutex<Vec<AgentEvent>>>,
}
impl AgentEmitter for Emitter {
    fn tool_program_observer(&self) -> Option<Arc<dyn ToolProgramObserver>> {
        self.observer.clone()
    }
    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        self.try_emit(event);
        Box::pin(async {})
    }
    fn try_emit(&self, event: AgentEvent) {
        self.events.lock().unwrap().push(event);
    }
}
async fn build(
    mut options: AgentHarnessOptions,
    log: Option<Arc<Log>>,
) -> (AgentHarness, Arc<Mutex<Vec<AgentEvent>>>) {
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("program", json!({}))
            .with_text("done"),
    );
    options.model = provider.default_model().clone();
    options.models = vec![provider];
    let events = Arc::new(Mutex::new(vec![]));
    options.agent_emitter = Some(Arc::new(Emitter {
        observer: log.map(|log| Arc::new(Observer(log)) as Arc<dyn ToolProgramObserver>),
        events: events.clone(),
    }));
    (AgentHarness::create(options).await.unwrap(), events)
}
async fn assert_no_child_messages(session: &Session, child_ids: &[String]) {
    let entries = session.find_entries(&EntryQuery::default()).await.unwrap();
    let mut root_results = 0;
    for entry in &entries {
        if let Entry::Message(entry) = entry {
            match &entry.message {
                AgentMessage::Assistant(message) => {
                    for content in &message.content {
                        if let Content::ToolCall(call) = content {
                            assert_eq!(call.name, "program", "no fabricated child assistant call");
                            assert!(!child_ids.contains(&call.id));
                        }
                    }
                }
                AgentMessage::ToolResult(result) => {
                    root_results += 1;
                    assert_eq!(result.tool_name, "program");
                    assert!(!child_ids.contains(&result.tool_call_id));
                }
                _ => {}
            }
        }
    }
    assert_eq!(root_results, 1);
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
async fn native_checkpoint_identity_partials_and_post_hook_final_are_observed() {
    let mut options = AgentHarnessOptions::default();
    let session = options.session.clone();
    let log = Log::new(session.clone(), PreparedFailure::None, None);
    let child_log = log.clone();
    let seen_reply = Arc::new(Mutex::new(Value::Null));
    let root_reply = seen_reply.clone();
    options.tools = vec![
        tool(
            "program",
            Arc::new(move |_, _, _, _| {
                let reply = root_reply.clone();
                Box::pin(async move {
                    assert!(current_observation().is_none());
                    let program = current().unwrap();
                    let id = start(&program, "effect", json!({"caller":"original"})).await;
                    let result = poll(&program, &id).await;
                    assert_eq!(result["status"], "done");
                    assert_eq!(result["isError"], true, "post-hook error override");
                    *reply.lock().unwrap() = result;
                    Ok(AgentToolResult::text("root done"))
                })
            }),
        ),
        native(
            "effect",
            json!({"type":"object"}),
            |args| {
                assert_eq!(args, json!({"caller":"original"}));
                Ok(json!({"prepared":"by-native-tool"}))
            },
            Arc::new(move |id, args, _, update| {
                let log = child_log.clone();
                Box::pin(async move {
                    assert_eq!(args, json!({"effective":"by-before-hook"}));
                    assert_eq!(log.phases(), vec!["begin", "prepared"]);
                    let child = log.children.lock().unwrap()[0].0.clone();
                    assert_eq!(child.request_id, id);
                    assert_eq!(child.parent_id, child.root_id);
                    assert_eq!(nested_parent().unwrap().root_id, child.root_id);
                    assert_ne!(current_observation().unwrap().id(), id);
                    log.event(json!({"phase":"execute","requestId":id}));
                    update(signed_partial());
                    Ok(AgentToolResult {
                        details: json!({"before":true}),
                        ..AgentToolResult::text("native final")
                    })
                })
            }),
        ),
    ];
    options.before_tool_call = Some(Arc::new(|ctx, _| {
        let rewrite = (ctx.tool_call.name == "effect").then(|| {
            assert_eq!(ctx.args, &json!({"prepared":"by-native-tool"}));
            BeforeToolCallResult {
                args: Some(json!({"effective":"by-before-hook"})),
                ..Default::default()
            }
        });
        Box::pin(async move { rewrite })
    }));
    let after_log = log.clone();
    options.after_tool_call = Some(Arc::new(move |ctx, _| {
        let result = (ctx.tool_call.name == "effect").then(|| {
            assert_eq!(
                ctx.result.content,
                AgentToolResult::text("native final").content
            );
            after_log.event(json!({"phase":"after-hook"}));
            AfterToolCallResult {
                content: Some(AgentToolResult::text("post-hook final").content),
                details: Some(json!({"after":{"authoritative":true}})),
                is_error: Some(true),
                usage: Some(patched_usage()),
                ..Default::default()
            }
        });
        Box::pin(async move { result })
    }));
    let (harness, events) = build(options, Some(log.clone())).await;
    assert!(matches!(
        bounded(harness.prompt_text("run", vec![]))
            .await
            .unwrap()
            .outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_eq!(
        log.phases(),
        vec![
            "begin",
            "prepared",
            "execute",
            "update",
            "after-hook",
            "finish",
            "closing",
            "closed"
        ]
    );
    let history = log.snapshot();
    assert_eq!(
        history[3]["partial"],
        serde_json::to_value(signed_partial()).unwrap()
    );
    assert_eq!(
        history[5]["result"]["content"][0]["text"],
        "post-hook final"
    );
    assert_eq!(
        history[5]["result"]["details"],
        json!({"after":{"authoritative":true}})
    );
    assert_eq!(
        history[5]["result"]["usage"],
        serde_json::to_value(patched_usage()).unwrap()
    );
    assert_eq!(history[5]["isError"], true);
    assert_eq!(history[5]["cancelled"], false);
    assert_eq!(seen_reply.lock().unwrap()["result"], history[5]["result"]);
    let child = log.children.lock().unwrap()[0].clone();
    assert_eq!(child.0.root_name, "program");
    assert_eq!(child.0.ordinal, 1);
    assert_eq!(child.1, json!({"caller":"original"}));
    assert_no_child_messages(&session, std::slice::from_ref(&child.0.request_id)).await;
    assert!(!events.lock().unwrap().iter().any(|e| matches!(e,
        AgentEvent::ToolExecutionStart { tool_call_id, .. } | AgentEvent::ToolExecutionEnd { tool_call_id, .. }
            if tool_call_id == &child.0.request_id)), "child observations are not transcript lifecycle events");
}

async fn prepared_failure(failure: PreparedFailure) {
    let mut options = AgentHarnessOptions::default();
    let session = options.session.clone();
    let close_gate = Arc::new(Notify::new());
    let log = Log::new(session.clone(), failure, Some(close_gate.clone()));
    let effects = Arc::new(AtomicUsize::new(0));
    let actual = effects.clone();
    let root_returned = Arc::new(Notify::new());
    let returned = root_returned.clone();
    options.tools = vec![
        tool(
            "program",
            Arc::new(move |_, _, _, _| {
                let returned = returned.clone();
                Box::pin(async move {
                    let program = current().unwrap();
                    let id = start(&program, "effect", json!({"requested":true})).await;
                    assert_eq!(poll(&program, &id).await["status"], "failed");
                    assert!(
                        program
                            .handle(json!({"op":"start","name":"effect","arguments":{}}))
                            .await
                            .is_err(),
                        "sink failure must close later admission"
                    );
                    returned.notify_one();
                    Ok(AgentToolResult::text(
                        "root returned, not necessarily completed",
                    ))
                })
            }),
        ),
        tool(
            "effect",
            Arc::new(move |_, _, _, _| {
                actual.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(AgentToolResult::text("must not execute")) })
            }),
        ),
    ];
    let after_log = log.clone();
    options.after_tool_call = Some(Arc::new(move |ctx, _| {
        if ctx.tool_call.name == "program" {
            after_log.event(json!({"phase":"root-after"}));
        }
        Box::pin(async { None })
    }));
    let (harness, _) = build(options, Some(log.clone())).await;
    let execution = tokio::spawn(async move { harness.prompt_text("run", vec![]).await });
    bounded(log.close_entered.notified()).await;
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert!(
        !execution.is_finished(),
        "native invocation owns observation drain"
    );
    assert!(!log.phases().contains(&"root-after".into()));
    close_gate.notify_one();
    bounded(root_returned.notified()).await;
    let result = bounded(execution).await.unwrap();
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    let phases = log.phases();
    assert_eq!(phases.iter().filter(|p| *p == "closed").count(), 1);
    assert!(
        phases.iter().position(|p| p == "closed").unwrap()
            < phases.iter().position(|p| p == "root-after").unwrap()
    );
    assert!(
        result.is_err(),
        "prepared {failure:?} must report persistence failure, not a false completed/aborted outcome: {result:?}"
    );
    let error = result.unwrap_err().to_string();
    assert!(error.contains("prepared"), "wrong error: {error}");
    let records = session.find_records(&RecordQuery::default()).await.unwrap();
    assert!(!records.iter().any(|r| matches!(r, LaneRecord::OperationFinished(r) if r.outcome == OperationOutcome::Completed)));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepared_sink_error_prevents_effects_and_later_starts_and_drains() {
    prepared_failure(PreparedFailure::Error).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepared_sink_panic_prevents_effects_and_later_starts_and_drains() {
    prepared_failure(PreparedFailure::Panic).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_denied_and_prepare_failure_finish_without_native_intent_or_execution() {
    let mut options = AgentHarnessOptions::default();
    let session = options.session.clone();
    let log = Log::new(session.clone(), PreparedFailure::None, None);
    let effects = Arc::new(AtomicUsize::new(0));
    let actual = effects.clone();
    let body: Body = Arc::new(move |_, _, _, _| {
        actual.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(AgentToolResult::text("must not execute")) })
    });
    options.tools = vec![
        tool(
            "program",
            Arc::new(|_, _, _, _| {
                Box::pin(async {
                    let program = current().unwrap();
                    for name in ["invalid", "denied", "prepare-failure"] {
                        let id = start(&program, name, json!({"caller":"original"})).await;
                        let reply = poll(&program, &id).await;
                        assert_eq!(reply["status"], "done");
                        assert_eq!(reply["isError"], true, "{name}: {reply}");
                    }
                    Ok(AgentToolResult::text("root done"))
                })
            }),
        ),
        native(
            "invalid",
            json!({"type":"object","properties":{"requiredNumber":{"type":"number"}},"required":["requiredNumber"]}),
            Ok,
            body.clone(),
        ),
        tool("denied", body.clone()),
        native(
            "prepare-failure",
            json!({"type":"object"}),
            |_| Err(AgentError::tool("controlled prepare_arguments failure")),
            body,
        ),
    ];
    options.before_tool_call = Some(Arc::new(|ctx, _| {
        let denied = (ctx.tool_call.name == "denied").then(|| BeforeToolCallResult {
            block: true,
            reason: Some("controlled policy denial".into()),
            ..Default::default()
        });
        Box::pin(async move { denied })
    }));
    let (harness, _) = build(options, Some(log.clone())).await;
    assert!(matches!(
        bounded(harness.prompt_text("run", vec![]))
            .await
            .unwrap()
            .outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    let events = log.snapshot();
    assert_eq!(
        log.phases(),
        ["begin", "finish", "closing", "closed"].repeat(3)
    );
    let finished: Vec<_> = events.iter().filter(|e| e["phase"] == "finish").collect();
    for event in &finished {
        assert_eq!(event["isError"], true);
        assert_eq!(event["cancelled"], false);
    }
    assert!(finished[0]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("requiredNumber"));
    assert!(finished[1]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("controlled policy denial"));
    assert!(finished[2]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("controlled prepare_arguments failure"));
    let entries = session.find_entries(&EntryQuery::default()).await.unwrap();
    assert!(!entries.iter().any(|e| matches!(e, Entry::Custom(e) if e.custom_type == "rpi.tool_program.intent.v1" || e.custom_type == "rpi.tool_program.finished.v1")));
    let ids: Vec<_> = log
        .children
        .lock()
        .unwrap()
        .iter()
        .map(|c| c.0.request_id.clone())
        .collect();
    assert_eq!(ids.len(), 3);
    assert_no_child_messages(&session, &ids).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_child_drains_native_execution_then_writer_before_root_finalization() {
    let mut options = AgentHarnessOptions::default();
    let session = options.session.clone();
    let close_gate = Arc::new(Notify::new());
    let log = Log::new(session, PreparedFailure::None, Some(close_gate.clone()));
    let started = Arc::new(Notify::new());
    let child_started = started.clone();
    let draining = Arc::new(Notify::new());
    let child_draining = draining.clone();
    let cleanup = Arc::new(Notify::new());
    let child_cleanup = cleanup.clone();
    let root_returned = Arc::new(Notify::new());
    let returned = root_returned.clone();
    let native_log = log.clone();
    options.tools = vec![
        tool(
            "program",
            Arc::new(move |_, _, _, _| {
                let started = started.clone();
                let returned = returned.clone();
                Box::pin(async move {
                    let program = current().unwrap();
                    let id = start(&program, "effect", json!({})).await;
                    started.notified().await;
                    program
                        .handle(json!({"op":"cancel","requestId":id}))
                        .await
                        .unwrap();
                    assert_eq!(poll(&program, &id).await["status"], "failed");
                    returned.notify_one();
                    Ok(AgentToolResult::text("root returned"))
                })
            }),
        ),
        tool(
            "effect",
            Arc::new(move |_, _, signal, update| {
                let started = child_started.clone();
                let draining = child_draining.clone();
                let cleanup = child_cleanup.clone();
                let log = native_log.clone();
                Box::pin(async move {
                    update(signed_partial());
                    started.notify_one();
                    signal.cancelled().await;
                    draining.notify_one();
                    cleanup.notified().await;
                    update(signed_partial()); // Cancelled cleanup updates must be suppressed.
                    log.event(json!({"phase":"native-drained"}));
                    Ok(AgentToolResult {
                        usage: Some(patched_usage()),
                        ..AgentToolResult::text("native cleanup result")
                    })
                })
            }),
        ),
    ];
    let after_log = log.clone();
    options.after_tool_call = Some(Arc::new(move |ctx, _| {
        if ctx.tool_call.name == "program" {
            after_log.event(json!({"phase":"root-after"}));
        }
        Box::pin(async { None })
    }));
    let (harness, _) = build(options, Some(log.clone())).await;
    let execution = tokio::spawn(async move { harness.prompt_text("run", vec![]).await });
    bounded(root_returned.notified()).await;
    bounded(draining.notified()).await;
    assert!(!execution.is_finished());
    assert!(!log
        .phases()
        .iter()
        .any(|p| p == "closing" || p == "root-after"));
    cleanup.notify_one();
    bounded(log.close_entered.notified()).await;
    assert!(!execution.is_finished());
    assert!(!log.phases().contains(&"root-after".into()));
    close_gate.notify_one();
    assert!(matches!(
        bounded(execution).await.unwrap().unwrap().outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    let phases = log.phases();
    let position = |phase| phases.iter().position(|p| p == phase).unwrap();
    assert!(position("native-drained") < position("finish"));
    assert!(position("closed") < position("root-after"));
    assert_eq!(phases.iter().filter(|p| *p == "update").count(), 1);
    let events = log.snapshot();
    let finish = events.iter().find(|e| e["phase"] == "finish").unwrap();
    assert_eq!(finish["cancelled"], true);
    assert_eq!(finish["isError"], false);
    assert_eq!(
        finish["result"]["content"][0]["text"],
        "native cleanup result"
    );
    assert_eq!(
        finish["result"]["usage"],
        serde_json::to_value(patched_usage()).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_journal_preserves_legacy_checkpoint_and_root_only_transcript() {
    let mut options = AgentHarnessOptions::default();
    let session = options.session.clone();
    let effect_session = session.clone();
    let effects = Arc::new(AtomicUsize::new(0));
    let actual = effects.clone();
    options.tools = vec![
        tool(
            "program",
            Arc::new(|_, _, _, _| {
                Box::pin(async {
                    let program = current().unwrap();
                    let id = start(&program, "effect", json!({})).await;
                    assert_eq!(poll(&program, &id).await["status"], "done");
                    Ok(AgentToolResult::text("root"))
                })
            }),
        ),
        tool(
            "effect",
            Arc::new(move |id, _, _, _| {
                let session = effect_session.clone();
                let effects = actual.clone();
                Box::pin(async move {
                    assert!(current_observation().unwrap().id().starts_with("tpchild-"));
                    assert!(session.find_entries(&EntryQuery::default()).await.unwrap().iter().any(|e| matches!(e,
                    Entry::Custom(e) if e.custom_type == "rpi.tool_program.intent.v1" && e.data.as_ref().unwrap()["requestId"] == id)));
                    effects.fetch_add(1, Ordering::SeqCst);
                    Ok(AgentToolResult::text("effect"))
                })
            }),
        ),
    ];
    let (harness, _) = build(options, None).await;
    assert!(matches!(
        bounded(harness.prompt_text("run", vec![]))
            .await
            .unwrap()
            .outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_no_child_messages(&session, &[]).await;
    let entries = session.find_entries(&EntryQuery::default()).await.unwrap();
    let begin = entries
        .iter()
        .find_map(|entry| match entry {
            Entry::Custom(entry) if entry.custom_type == "rpi.tool_program.child_begin.v1" => {
                entry.data.as_ref()
            }
            _ => None,
        })
        .expect("default native child begin was committed");
    assert_eq!(begin["toolName"], "effect");
    assert!(begin["childId"].as_str().unwrap().starts_with("tpchild-"));
    assert!(entries.iter().any(|entry| matches!(entry,
        Entry::Custom(entry) if entry.custom_type == "rpi.tool_program.child_phase.v1"
            && entry.data.as_ref().unwrap()["phase"] == "final")));
}

async fn journal(session: &Session) -> (Vec<Value>, Vec<Value>) {
    let entries = session
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut begins = vec![];
    let mut phases = vec![];
    for entry in entries {
        if let Entry::Custom(entry) = entry {
            match entry.custom_type.as_str() {
                "rpi.tool_program.child_begin.v1" => begins.push(entry.data.unwrap()),
                "rpi.tool_program.child_phase.v1" => phases.push(entry.data.unwrap()),
                _ => {}
            }
        }
    }
    (begins, phases)
}
async fn journal_body(session: &Session, descriptor: &Value) -> Value {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use rpi_harness::tool_program_body::{read_body_page, BodyPageQuery, MAX_PAGE_BYTES};
    use sha2::{Digest, Sha256};
    let mut query = BodyPageQuery {
        descriptor_id: descriptor["descriptorId"].as_str().unwrap().into(),
        leaf_id: None,
        cursor: None,
        limit: 8,
        max_bytes: MAX_PAGE_BYTES,
    };
    let mut bytes = vec![];
    loop {
        let page = read_body_page(session, "main", &query).await.unwrap();
        for chunk in page.chunks {
            bytes.extend(STANDARD.decode(chunk.data).unwrap());
        }
        query.cursor = page.next_cursor;
        if query.cursor.is_none() {
            break;
        }
    }
    assert_eq!(
        bytes.len() as u64,
        descriptor["byteLength"].as_u64().unwrap()
    );
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        descriptor["sha256"]
    );
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_journal_parallel_cold_reopen_signed_partials_and_final_no_replay() {
    use rpi_harness::session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        DefaultIdGenerator, SystemClock,
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("journal.jsonl")
        .to_string_lossy()
        .replace(char::from(92), "/");
    let fs: Arc<dyn rpi_tools::FileSystem> =
        Arc::new(rpi_tools::OsExecutionEnv::with_cwd(dir.path().into()));
    let clock = Arc::new(SystemClock);
    let ids = Arc::new(DefaultIdGenerator::new());
    let storage = JsonlSessionStorage::create(
        fs.clone(),
        &path,
        JsonlV4Header::new("journal-cold".into(), 1, "/".into(), None, None, None),
        clock.clone(),
        ids.clone(),
    )
    .await
    .unwrap();
    let session = Session::new(Arc::new(storage), None);
    let effects = Arc::new(AtomicUsize::new(0));
    let count = effects.clone();
    let mut options = AgentHarnessOptions {
        session: session.clone(),
        ..Default::default()
    };
    options.tools = vec![
        tool(
            "program",
            Arc::new(|_, _, _, _| {
                Box::pin(async {
                    let program = current().unwrap();
                    let mut calls = vec![];
                    for n in 0..8 {
                        calls.push(start(&program, "effect", json!({"n":n})).await);
                    }
                    for call in calls {
                        assert_eq!(poll(&program, &call).await["status"], "done");
                    }
                    Ok(AgentToolResult::text("root"))
                })
            }),
        ),
        tool(
            "effect",
            Arc::new(move |_, args, _, update| {
                let count = count.clone();
                Box::pin(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    for n in 0..4 {
                        let mut partial = signed_partial();
                        partial.details =
                            json!({"n":args["n"], "update":n, "padding":"中".repeat(12_000)});
                        update(partial);
                    }
                    // Deliberately no await/flush between partials and native finished.
                    Ok(AgentToolResult::text("pre-hook"))
                })
            }),
        ),
    ];
    options.after_tool_call = Some(Arc::new(|ctx, _| {
        let result = (ctx.tool_call.name == "effect").then(|| AfterToolCallResult {
            content: Some(signed_partial().content),
            details: Some(json!({"authoritative":true})),
            is_error: Some(true),
            usage: Some(patched_usage()),
            ..Default::default()
        });
        Box::pin(async move { result })
    }));
    let (harness, _) = build(options, None).await;
    assert!(matches!(
        bounded(harness.prompt_text("run", vec![]))
            .await
            .unwrap()
            .outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_eq!(effects.load(Ordering::SeqCst), 8);
    drop(harness);
    session.storage().drain().await;
    drop(session);
    let reopened = Session::new(
        Arc::new(
            JsonlSessionStorage::load(fs, &path, clock, ids)
                .await
                .unwrap(),
        ),
        None,
    );
    let (begins, phases) = journal(&reopened).await;
    assert_eq!(begins.len(), 8);
    assert_eq!(phases.len(), 8 * 8);
    for begin in begins {
        let child = begin["childId"].as_str().unwrap();
        assert_eq!(begin["toolName"], "effect");
        assert!(begin["parentChildId"].is_null());
        assert!(!begin.to_string().contains("tp-"));
        let child_phases: Vec<_> = phases.iter().filter(|p| p["childId"] == child).collect();
        assert_eq!(
            child_phases
                .iter()
                .map(|p| p["phase"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "requested",
                "prepared",
                "update",
                "update",
                "update",
                "update",
                "final",
                "closed"
            ]
        );
        for (index, phase) in child_phases.iter().enumerate() {
            assert_eq!(phase["index"], index as u64);
        }
        let requested = journal_body(&reopened, &child_phases[0]["descriptor"]).await;
        assert_eq!(
            journal_body(&reopened, &child_phases[1]["descriptor"]).await,
            requested
        );
        for n in 0..4 {
            let partial = journal_body(&reopened, &child_phases[n + 2]["descriptor"]).await;
            assert_eq!(partial["content"][0]["textSignature"], "native-signature");
            assert_eq!(partial["details"]["update"], n as u64);
            assert_eq!(partial["details"]["n"], requested["n"]);
        }
        let final_result = journal_body(&reopened, &child_phases[6]["descriptor"]).await;
        assert_eq!(
            final_result["content"],
            serde_json::to_value(signed_partial().content).unwrap()
        );
        assert_eq!(final_result["details"], json!({"authoritative":true}));
        assert_eq!(
            final_result["usage"],
            serde_json::to_value(patched_usage()).unwrap()
        );
        assert_eq!(child_phases[6]["isError"], true);
        assert_eq!(child_phases[7]["failed"], false);
        assert_eq!(child_phases[7]["interrupted"], false);
    }
    assert_no_child_messages(&reopened, &[]).await;
    // Loading the owning native log is read-only: no uncertain execution replay.
    assert_eq!(effects.load(Ordering::SeqCst), 8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_journal_nested_public_parent_is_real_child_not_private_request() {
    let mut options = AgentHarnessOptions::default();
    let session = options.session.clone();
    options.tools = vec![
        tool(
            "program",
            Arc::new(|_, _, _, _| {
                Box::pin(async {
                    let program = current().unwrap();
                    let id = start(&program, "wrapper", json!({})).await;
                    assert_eq!(poll(&program, &id).await["status"], "done");
                    Ok(AgentToolResult::text("root"))
                })
            }),
        ),
        tool(
            "wrapper",
            Arc::new(|_, _, _, _| {
                Box::pin(async {
                    let program = current().unwrap();
                    let id = start(&program, "effect", json!({})).await;
                    assert_eq!(poll(&program, &id).await["status"], "done");
                    Ok(AgentToolResult::text("wrapper"))
                })
            }),
        ),
        tool(
            "effect",
            Arc::new(|_, _, _, _| Box::pin(async { Ok(signed_partial()) })),
        ),
    ];
    let (harness, _) = build(options, None).await;
    bounded(harness.prompt_text("run", vec![])).await.unwrap();
    let (begins, phases) = journal(&session).await;
    assert_eq!(begins.len(), 2);
    assert_eq!(begins[0]["toolName"], "wrapper");
    assert!(begins[0]["parentChildId"].is_null());
    assert_eq!(begins[1]["parentChildId"], begins[0]["childId"]);
    assert_eq!(begins[1]["origin"], begins[0]["origin"]);
    assert_eq!(begins[1]["rootCallId"], begins[0]["rootCallId"]);
    assert_eq!(phases.iter().filter(|p| p["phase"] == "final").count(), 2);
    assert_no_child_messages(&session, &[]).await;
}

#[tokio::test]
async fn public_append_cannot_fabricate_native_child_namespace() {
    use rpi_harness::agent_harness::{BranchWriteGuard, ScopedEntry};
    let session = AgentHarnessOptions::default().session;
    session.create_lane("side", None).await.unwrap();
    let harness = AgentHarness::create(AgentHarnessOptions {
        session: session.clone(),
        ..Default::default()
    })
    .await
    .unwrap();
    for custom_type in [
        "rpi.tool_program.child_begin.v1",
        "rpi.tool_program.child_phase.v1",
    ] {
        for lane in ["main"] {
            let revision = session.storage().history_revision().await.unwrap();
            let guard = BranchWriteGuard {
                session_id: session.storage().metadata().id,
                leaf_id: session.view(lane).get_leaf_id().await.unwrap(),
                custom_type: custom_type.into(),
                entry_id: None,
            };
            let error = harness
                .append_scoped_entry(
                    &guard,
                    ScopedEntry::Custom {
                        custom_type: custom_type.into(),
                        data: Some(json!({"version":1,"childId":"forged"})),
                    },
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("reserved native child journal"));
            assert_eq!(
                session.storage().history_revision().await.unwrap(),
                revision
            );
        }
        assert!(session
            .append_custom_entry(custom_type, Some(json!({})))
            .await
            .is_err());
        assert!(session
            .view("side")
            .append_custom_entry(custom_type, Some(json!({})))
            .await
            .is_err());
        assert!(session
            .append_entry(
                ProvisionedEntry {
                    id: "forged".into(),
                    kind: ProvisionedKind::Custom {
                        custom_type: custom_type.into(),
                        data: Some(json!({}))
                    }
                },
                "main"
            )
            .await
            .is_err());
    }
    for lane in ["main"] {
        let custom_type = "rpi.tool_program.child_begin.v1.other";
        let guard = BranchWriteGuard {
            session_id: session.storage().metadata().id,
            leaf_id: session.view(lane).get_leaf_id().await.unwrap(),
            custom_type: custom_type.into(),
            entry_id: None,
        };
        harness
            .append_scoped_entry(
                &guard,
                ScopedEntry::Custom {
                    custom_type: custom_type.into(),
                    data: None,
                },
            )
            .await
            .unwrap();
    }
    assert_eq!(
        session
            .find_entries(&EntryQuery::default())
            .await
            .unwrap()
            .len(),
        1
    );
}
