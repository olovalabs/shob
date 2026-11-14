//! Native observation only: these tests deliberately create no child AgentMessages.
#[path = "common/mod.rs"]
mod common;
use futures::future::BoxFuture;
use rpi_agent::{
    tool_program::{current, nested_parent, ToolProgram},
    tool_program_observation::{
        current_observation, ToolProgramChild, ToolProgramObservation, ToolProgramObserver,
    },
    AfterToolCallResult, AgentContext, AgentEmitter, AgentError, AgentEvent, AgentLoopConfig,
    AgentMessage, AgentTool, AgentToolResult, BeforeToolCallResult, ToolExecutionMode,
};
use rpi_ai::types::{StopReason, Tool, ToolResultMessage, Usage};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Condvar, Mutex, Weak,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

type Update = Arc<dyn Fn(AgentToolResult) + Send + Sync>;
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
struct NativeTool {
    schema: Tool,
    body: Body,
    prepared_errors: Arc<Mutex<Vec<String>>>,
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
        ToolExecutionMode::Sequential
    }
    async fn execute(
        &self,
        id: &str,
        args: Value,
        signal: CancellationToken,
        update: Update,
    ) -> Result<AgentToolResult, AgentError> {
        // Simulate the native harness barrier. The seam intentionally does not
        // call prepared itself (denied/invalid calls never reach this barrier).
        if let Some(observation) = current_observation() {
            if let Err(error) = observation.prepared(&args).await {
                self.prepared_errors.lock().unwrap().push(error.clone());
                return Err(AgentError::tool(error));
            }
        }
        (self.body)(id.into(), args, signal, update).await
    }
}
fn tool(name: &str, body: Body) -> Arc<dyn AgentTool> {
    tool_with_prepared_errors(name, body).0
}
fn tool_with_prepared_errors(
    name: &str,
    body: Body,
) -> (Arc<dyn AgentTool>, Arc<Mutex<Vec<String>>>) {
    let errors = Arc::new(Mutex::new(Vec::new()));
    (
        Arc::new(NativeTool {
            schema: serde_json::from_value(
                json!({"name":name,"description":"test","parameters":{"type":"object"}}),
            )
            .unwrap(),
            body,
            prepared_errors: errors.clone(),
        }),
        errors,
    )
}
#[derive(Default)]
struct Log {
    children: Mutex<Vec<(ToolProgramChild, Value)>>,
    events: Mutex<Vec<Value>>,
    weak: Mutex<Vec<Weak<dyn ToolProgramObservation>>>,
    fail: Mutex<Option<&'static str>>,
    begin_entered: Notify,
    begin_gate: Option<Arc<Notify>>,
    close_entered: Notify,
    close_gate: Option<Arc<Notify>>,
    update_entered: Notify,
    update_gate: Option<Arc<(Mutex<bool>, Condvar)>>,
}
impl Log {
    fn event(&self, value: Value) {
        self.events.lock().unwrap().push(value);
    }
    fn fails(&self, phase: &str) -> bool {
        *self.fail.lock().unwrap() == Some(phase)
    }
    fn snapshot(&self) -> Vec<Value> {
        self.events.lock().unwrap().clone()
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
            self.0.event(json!({"phase":"begin","name":child.name}));
            if self.0.fails("begin") {
                return Err("writer begin failed".into());
            }
            if self.0.fails("begin_panic") {
                panic!("writer begin panic");
            }
            let obs: Arc<dyn ToolProgramObservation> = Arc::new(Observation {
                id: format!("entry-{}", child.ordinal),
                child: child.clone(),
                log: self.0.clone(),
            });
            self.0.weak.lock().unwrap().push(Arc::downgrade(&obs));
            self.0.children.lock().unwrap().push((child, args.clone()));
            self.0.begin_entered.notify_one();
            if let Some(gate) = &self.0.begin_gate {
                gate.notified().await;
            }
            Ok(obs)
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
        self.log.update_entered.notify_one();
        // Test-only synchronous latch: models a callback descheduled after
        // admission, not asynchronous I/O in the nonblocking writer contract.
        if let Some(gate) = &self.log.update_gate {
            let (released, wake) = &**gate;
            let mut released = released.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
        }
        self.log
            .event(json!({"phase":"update","name":self.child.name,"partial":partial}));
        if self.log.fails("update") {
            Err("writer update failed".into())
        } else {
            Ok(())
        }
    }
    fn prepared<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, Result<(), String>> {
        if self.log.fails("prepared_construct_panic") {
            panic!("writer prepared construction panic");
        }
        Box::pin(async move {
            if self.log.fails("prepared_poll_panic") {
                panic!("writer prepared polling panic");
            }
            self.log
                .event(json!({"phase":"prepared","name":self.child.name,"args":args}));
            if self.log.fails("prepared") {
                Err("writer prepared failed".into())
            } else {
                Ok(())
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
            self.log.event(json!({"phase":"finish","name":self.child.name,"result":result,"isError":is_error,"cancelled":cancelled}));
            if self.log.fails("finish_panic") {
                panic!("writer finish panic");
            }
            if self.log.fails("finish") {
                Err("writer finish failed".into())
            } else {
                Ok(())
            }
        })
    }
    fn close<'a>(&'a self, reason: Option<&'a str>) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.log
                .event(json!({"phase":"closing","name":self.child.name,"reason":reason}));
            self.log.close_entered.notify_one();
            if let Some(gate) = &self.log.close_gate {
                gate.notified().await;
            }
            self.log
                .event(json!({"phase":"closed","name":self.child.name}));
            if self.log.fails("close") {
                Err("writer close failed".into())
            } else {
                Ok(())
            }
        })
    }
}
struct Emitter {
    observer: Arc<Observer>,
    events: Arc<Mutex<Vec<AgentEvent>>>,
}
impl AgentEmitter for Emitter {
    fn tool_program_observer(&self) -> Option<Arc<dyn ToolProgramObserver>> {
        Some(self.observer.clone())
    }
    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        self.try_emit(event);
        Box::pin(async {})
    }
    fn try_emit(&self, event: AgentEvent) {
        self.events.lock().unwrap().push(event);
    }
}
async fn run(
    tools: Vec<Arc<dyn AgentTool>>,
    cfg: AgentLoopConfig,
    log: Arc<Log>,
) -> (Vec<AgentEvent>, ToolResultMessage) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let output = rpi_agent::run_agent_loop(
        vec![common::user_message("run")],
        AgentContext {
            tools,
            ..Default::default()
        },
        cfg,
        Arc::new(Emitter {
            observer: Arc::new(Observer(log)),
            events: events.clone(),
        }),
        common::mock_stream_fn(vec![
            common::assistant_tool_calls(
                vec![("root-call", "program", json!({}))],
                StopReason::ToolUse,
            ),
            common::assistant_text("done", StopReason::Stop),
        ]),
    )
    .await
    .unwrap();
    let results: Vec<_> = output
        .into_iter()
        .filter_map(|m| match m {
            AgentMessage::ToolResult(r) => Some(*r),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1, "no fabricated child transcript");
    let events = events.lock().unwrap().clone();
    (events, results.into_iter().next().unwrap())
}
async fn start(p: &Arc<ToolProgram>, name: &str) -> Value {
    p.handle(json!({"op":"start","name":name,"arguments":{"requested":true}}))
        .await
        .unwrap()["requestId"]
        .clone()
}
async fn poll(p: &Arc<ToolProgram>, id: &Value) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let reply = p.handle(json!({"op":"poll","requestId":id})).await.unwrap();
            if reply["status"] != "running" {
                return reply;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
fn metadata(terminate: bool) -> AgentToolResult {
    let mut usage = Usage::zero();
    usage.input = 3;
    usage.total_tokens = 3;
    AgentToolResult {
        usage: Some(usage),
        added_tool_names: vec!["native-added".into()],
        terminate,
        ..AgentToolResult::text("actual")
    }
}
fn signed() -> AgentToolResult {
    AgentToolResult {
        content: vec![rpi_agent::TextContentOrImage::Text(
            serde_json::from_value(
                json!({"type":"text","text":"actual partial","textSignature":"signature"}),
            )
            .unwrap(),
        )],
        details: json!({"secret":{"payload":[1,2,3]}}),
        ..Default::default()
    }
}

#[tokio::test]
async fn real_partials_and_post_hook_result_keep_private_identity_and_weak_callbacks() {
    let log = Arc::new(Log::default());
    let saved: Arc<Mutex<Option<Update>>> = Arc::new(Mutex::new(None));
    let retained = saved.clone();
    let mut cfg = common::base_config();
    cfg.before_tool_call = Some(Arc::new(|ctx, _| {
        let rewrite = (ctx.tool_call.name == "effect").then(|| BeforeToolCallResult {
            args: Some(json!({"prepared":"rewritten"})),
            ..Default::default()
        });
        Box::pin(async move { rewrite })
    }));
    cfg.after_tool_call = Some(Arc::new(|ctx, _| {
        let rewrite = (ctx.tool_call.name == "effect").then(|| AfterToolCallResult {
            content: Some(AgentToolResult::text("AFTER hook").content),
            details: Some(json!({"after":true})),
            ..Default::default()
        });
        Box::pin(async move { rewrite })
    }));
    let (events, result) = run(
        vec![
            tool(
                "program",
                Arc::new(|_, _, _, _| {
                    Box::pin(async {
                        assert!(current_observation().is_none());
                        let p = current().unwrap();
                        let id = start(&p, "effect").await;
                        let reply = poll(&p, &id).await;
                        assert_eq!(reply["status"], "done");
                        assert_eq!(reply["result"]["content"][0]["text"], "AFTER hook");
                        Ok(AgentToolResult::text("root"))
                    })
                }),
            ),
            tool(
                "effect",
                Arc::new(move |id, args, _, update| {
                    *retained.lock().unwrap() = Some(update.clone());
                    Box::pin(async move {
                        assert!(id.starts_with("tp-"));
                        assert_ne!(current_observation().unwrap().id(), id);
                        assert_eq!(nested_parent().unwrap().parent_id, "root-call");
                        assert_eq!(args, json!({"prepared":"rewritten"}));
                        update(signed());
                        Ok(metadata(false))
                    })
                }),
            ),
        ],
        cfg,
        log.clone(),
    )
    .await;
    assert_eq!(result.usage.unwrap().total_tokens, 3);
    let children = log.children.lock().unwrap();
    assert_eq!(children.len(), 1);
    let (child, args) = &children[0];
    assert_eq!(
        (
            child.root_id.as_str(),
            child.root_name.as_str(),
            child.parent_id.as_str(),
            child.name.as_str(),
            child.ordinal
        ),
        ("root-call", "program", "root-call", "effect", 1)
    );
    assert_eq!(args, &json!({"requested":true}));
    drop(children);
    let history = log.snapshot();
    assert_eq!(
        history
            .iter()
            .map(|e| e["phase"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["begin", "prepared", "update", "finish", "closing", "closed"]
    );
    assert_eq!(
        history[2]["partial"],
        serde_json::to_value(signed()).unwrap()
    );
    assert_eq!(history[3]["result"]["details"], json!({"after":true}));
    assert_eq!(history[3]["result"]["content"][0]["text"], "AFTER hook");
    assert_eq!(history[3]["isError"], false);
    assert_eq!(history[3]["cancelled"], false);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolExecutionUpdate { .. }))
            .count(),
        3,
        "legacy lifecycle count unchanged"
    );
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while log
            .weak
            .lock()
            .unwrap()
            .iter()
            .any(|w| w.strong_count() != 0)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    saved.lock().unwrap().as_ref().unwrap()(signed());
    assert_eq!(
        history,
        log.snapshot(),
        "retained callback neither owns nor updates writer"
    );
}

#[tokio::test]
async fn nested_children_share_real_start_ordinals_and_close_before_parent_finish() {
    let log = Arc::new(Log::default());
    run(
        vec![
            tool(
                "program",
                Arc::new(|_, _, _, _| {
                    Box::pin(async {
                        let p = current().unwrap();
                        let id = start(&p, "nested").await;
                        assert_eq!(poll(&p, &id).await["status"], "done");
                        let id = start(&p, "effect").await;
                        assert_eq!(poll(&p, &id).await["status"], "done");
                        Ok(AgentToolResult::text("root"))
                    })
                }),
            ),
            tool(
                "nested",
                Arc::new(|id, _, _, _| {
                    Box::pin(async move {
                        assert!(id.starts_with("tp-"));
                        let p = current().unwrap();
                        let leaf = start(&p, "effect").await;
                        assert_eq!(poll(&p, &leaf).await["status"], "done");
                        Ok(AgentToolResult::text("nested"))
                    })
                }),
            ),
            tool(
                "effect",
                Arc::new(|_, _, _, _| Box::pin(async { Ok(AgentToolResult::text("leaf")) })),
            ),
        ],
        common::base_config(),
        log.clone(),
    )
    .await;
    let children = log.children.lock().unwrap();
    assert_eq!(
        children.iter().map(|c| c.0.ordinal).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(children[1].0.parent_id, children[0].0.request_id);
    assert_eq!(children[2].0.parent_id, "root-call");
    assert!(children
        .iter()
        .all(|c| c.0.root_id == "root-call" && c.0.root_name == "program"));
    let events = log.snapshot();
    let leaf_closed = events
        .iter()
        .position(|e| e["phase"] == "closed" && e["name"] == "effect")
        .unwrap();
    let parent_finish = events
        .iter()
        .position(|e| e["phase"] == "finish" && e["name"] == "nested")
        .unwrap();
    assert!(leaf_closed < parent_finish);
}

#[tokio::test]
async fn denied_calls_are_observed_without_prepared_or_side_effects() {
    let log = Arc::new(Log::default());
    let effects = Arc::new(AtomicUsize::new(0));
    let actual = effects.clone();
    let mut cfg = common::base_config();
    cfg.before_tool_call = Some(Arc::new(|ctx, _| {
        let denied = (ctx.tool_call.name == "effect").then(|| BeforeToolCallResult {
            block: true,
            reason: Some("denied".into()),
            ..Default::default()
        });
        Box::pin(async move { denied })
    }));
    run(
        vec![
            tool(
                "program",
                Arc::new(|_, _, _, _| {
                    Box::pin(async {
                        let p = current().unwrap();
                        let id = start(&p, "effect").await;
                        let reply = poll(&p, &id).await;
                        assert_eq!(reply["isError"], true);
                        Ok(AgentToolResult::text("root"))
                    })
                }),
            ),
            tool(
                "effect",
                Arc::new(move |_, _, _, _| {
                    actual.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(metadata(false)) })
                }),
            ),
        ],
        cfg,
        log.clone(),
    )
    .await;
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    let events = log.snapshot();
    assert!(!events.iter().any(|e| e["phase"] == "prepared"));
    assert_eq!(events[1]["phase"], "finish");
    assert_eq!(events[1]["isError"], true);
    assert!(events[1]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("denied"));
}

#[tokio::test]
async fn writer_failures_fail_closed_without_losing_known_accounting() {
    for phase in [
        "begin",
        "begin_panic",
        "prepared",
        "prepared_construct_panic",
        "prepared_poll_panic",
        "update",
        "finish",
        "finish_panic",
        "close",
    ] {
        let log = Arc::new(Log::default());
        *log.fail.lock().unwrap() = Some(phase);
        let effects = Arc::new(AtomicUsize::new(0));
        let actual = effects.clone();
        let (effect, prepared_errors) = tool_with_prepared_errors(
            "effect",
            Arc::new(move |_, _, _, update| {
                actual.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    update(signed());
                    update(signed());
                    Ok(metadata(false))
                })
            }),
        );
        let (_, result) = run(
            vec![
                tool(
                    "program",
                    Arc::new(|_, _, _, _| {
                        Box::pin(async {
                            let p = current().unwrap();
                            let id = start(&p, "effect").await;
                            assert_eq!(poll(&p, &id).await["status"], "failed");
                            assert!(p
                                .handle(json!({"op":"start","name":"effect","arguments":{}}))
                                .await
                                .is_err());
                            Ok(AgentToolResult::text("root"))
                        })
                    }),
                ),
                effect,
            ],
            common::base_config(),
            log.clone(),
        )
        .await;
        if phase.starts_with("prepared") {
            let errors = prepared_errors.lock().unwrap();
            assert_eq!(
                errors.len(),
                1,
                "the real barrier must receive Err, not unwind: {phase}"
            );
            assert!(errors[0].contains("prepared"));
        }
        let executed = !matches!(
            phase,
            "begin"
                | "begin_panic"
                | "prepared"
                | "prepared_construct_panic"
                | "prepared_poll_panic"
        );
        assert_eq!(
            effects.load(Ordering::SeqCst),
            usize::from(executed),
            "{phase}"
        );
        assert_eq!(
            result.usage.map(|u| u.total_tokens),
            executed.then_some(3),
            "{phase}"
        );
        if executed {
            assert_eq!(result.added_tool_names, vec!["native-added"]);
        }
        let events = log.snapshot();
        let closes = events.iter().filter(|e| e["phase"] == "closed").count();
        assert_eq!(
            closes,
            usize::from(!matches!(phase, "begin" | "begin_panic")),
            "{phase}"
        );
        if phase == "update" {
            assert_eq!(
                events.iter().filter(|e| e["phase"] == "update").count(),
                1,
                "failed writer receives no replay or later updates"
            );
            let finished = events.iter().find(|e| e["phase"] == "finish").unwrap();
            assert_eq!(finished["result"]["content"][0]["text"], "actual");
            assert_eq!(finished["cancelled"], true);
            assert_eq!(
                finished["isError"], false,
                "finish describes native result, not broker failure"
            );
        }
    }
}

#[tokio::test]
async fn terminal_accounting_is_retained_before_final_trace_failure_and_broker_overflow() {
    for fail in [Some("finish"), Some("finish_panic"), None] {
        let log = Arc::new(Log::default());
        *log.fail.lock().unwrap() = fail;
        let (_, result) = run(
            vec![
                tool(
                    "program",
                    Arc::new(|_, _, _, _| {
                        Box::pin(async {
                            let p = current().unwrap();
                            let id = start(&p, "effect").await;
                            let reply = poll(&p, &id).await;
                            assert_eq!(reply["status"], "failed");
                            assert!(p
                                .handle(json!({"op":"start","name":"effect","arguments":{}}))
                                .await
                                .is_err());
                            Ok(AgentToolResult::text("root"))
                        })
                    }),
                ),
                tool(
                    "effect",
                    Arc::new(|_, _, _, _| {
                        Box::pin(async {
                            let mut result = metadata(true);
                            result.details = json!({"large":"x".repeat(1024*1024+1)});
                            Ok(result)
                        })
                    }),
                ),
            ],
            common::base_config(),
            log.clone(),
        )
        .await;
        assert_eq!(result.usage.unwrap().total_tokens, 3);
        assert_eq!(result.added_tool_names, vec!["native-added"]);
        let events = log.snapshot();
        let finish = events.iter().find(|e| e["phase"] == "finish").unwrap();
        assert_eq!(finish["result"]["terminate"], true);
        assert_eq!(
            finish["result"]["details"]["large"].as_str().unwrap().len(),
            1024 * 1024 + 1
        );
        assert_eq!(events.iter().filter(|e| e["phase"] == "closed").count(), 1);
        // The public ToolResultMessage has no terminate field. The loop stopping
        // before the scripted follow-up and the denied start cover its latch.
    }
}

#[tokio::test]
async fn cancellation_replies_promptly_but_parent_waits_for_native_and_writer_drains() {
    for mode in ["cancel", "timeout"] {
        let close_gate = Arc::new(Notify::new());
        let log = Arc::new(Log {
            close_gate: Some(close_gate.clone()),
            ..Default::default()
        });
        let started = Arc::new(Notify::new());
        let child_started = started.clone();
        let draining = Arc::new(Notify::new());
        let child_draining = draining.clone();
        let cleanup = Arc::new(Notify::new());
        let child_cleanup = cleanup.clone();
        let parent_returned = Arc::new(Notify::new());
        let returned = parent_returned.clone();
        let native_log = log.clone();
        let after_log = log.clone();
        let mut cfg = common::base_config();
        cfg.after_tool_call = Some(Arc::new(move |ctx, _| {
            if ctx.tool_call.name == "program" {
                after_log.event(json!({"phase":"root-after"}));
            }
            Box::pin(async { None })
        }));
        let execution = tokio::spawn(run(
            vec![
                tool(
                    "program",
                    Arc::new(move |_, _, _, _| {
                        let started = started.clone();
                        let returned = returned.clone();
                        Box::pin(async move {
                            let p = current().unwrap();
                            let id = p.handle(json!({"op":"start","name":"effect","arguments":{},"timeoutMs":if mode == "timeout" {100} else {30000}})).await.unwrap()["requestId"].clone();
                            started.notified().await;
                            if mode == "cancel" {
                                p.handle(json!({"op":"cancel","requestId":id}))
                                    .await
                                    .unwrap();
                            }
                            assert_eq!(poll(&p, &id).await["status"], "failed");
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
                            update(signed());
                            started.notify_one();
                            signal.cancelled().await;
                            draining.notify_one();
                            cleanup.notified().await;
                            update(signed()); // Cancelled cleanup payload must be suppressed.
                            log.event(json!({"phase":"native-drained"}));
                            Ok(metadata(false))
                        })
                    }),
                ),
            ],
            cfg,
            log.clone(),
        ));
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            parent_returned.notified(),
        )
        .await
        .unwrap();
        draining.notified().await;
        assert!(!execution.is_finished());
        assert!(!log.snapshot().iter().any(|e| e["phase"] == "closing"));
        cleanup.notify_one();
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            log.close_entered.notified(),
        )
        .await
        .unwrap();
        assert!(
            !execution.is_finished(),
            "active permit must survive writer close"
        );
        assert!(!log.snapshot().iter().any(|e| e["phase"] == "root-after"));
        close_gate.notify_one();
        let (_, result) = tokio::time::timeout(std::time::Duration::from_secs(3), execution)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.usage.unwrap().total_tokens, 3);
        let events = log.snapshot();
        let position = |phase| events.iter().position(|e| e["phase"] == phase).unwrap();
        assert!(position("native-drained") < position("finish"));
        assert!(position("closed") < position("root-after"));
        let finish = &events[position("finish")];
        assert_eq!(finish["cancelled"], true);
        assert_eq!(finish["isError"], false);
        assert_eq!(events.iter().filter(|e| e["phase"] == "update").count(), 1);
        assert_eq!(events.iter().filter(|e| e["phase"] == "closed").count(), 1);
    }
}

#[tokio::test]
async fn panicking_native_parent_drains_descendant_and_closes_both_observations() {
    let log = Arc::new(Log::default());
    let ready = Arc::new(Notify::new());
    let leaf_ready = ready.clone();
    let native_log = log.clone();
    let after_log = log.clone();
    let mut cfg = common::base_config();
    cfg.after_tool_call = Some(Arc::new(move |ctx, _| {
        if ctx.tool_call.name == "program" {
            after_log.event(json!({"phase":"root-after"}));
        }
        Box::pin(async { None })
    }));
    let (_, result) = run(
        vec![
            tool(
                "program",
                Arc::new(|_, _, _, _| {
                    Box::pin(async {
                        let p = current().unwrap();
                        let id = start(&p, "nested").await;
                        assert_eq!(poll(&p, &id).await["status"], "failed");
                        Ok(AgentToolResult::text("root"))
                    })
                }),
            ),
            tool(
                "nested",
                Arc::new(move |_, _, _, _| {
                    let ready = ready.clone();
                    Box::pin(async move {
                        let p = current().unwrap();
                        start(&p, "effect").await;
                        ready.notified().await;
                        panic!("native nested panic");
                    })
                }),
            ),
            tool(
                "effect",
                Arc::new(move |_, _, signal, _| {
                    let ready = leaf_ready.clone();
                    let log = native_log.clone();
                    Box::pin(async move {
                        ready.notify_one();
                        signal.cancelled().await;
                        log.event(json!({"phase":"native-drained"}));
                        Ok(metadata(false))
                    })
                }),
            ),
        ],
        cfg,
        log.clone(),
    )
    .await;
    // A panicking parent has no actual final result to fabricate. Its known
    // descendant result is traced before parent close, not invented as a parent
    // result/accounting record by this seam.
    assert!(result.usage.is_none());
    let events = log.snapshot();
    let position = |phase, name| {
        events
            .iter()
            .position(|e| e["phase"] == phase && e["name"] == name)
            .unwrap()
    };
    assert!(position("closed", "effect") < position("closing", "nested"));
    assert!(!events
        .iter()
        .any(|e| e["phase"] == "finish" && e["name"] == "nested"));
    assert_eq!(events.iter().filter(|e| e["phase"] == "closed").count(), 2);
    assert!(
        position("closed", "nested")
            < events
                .iter()
                .position(|e| e["phase"] == "root-after")
                .unwrap()
    );
}

#[tokio::test]
async fn cancelling_during_begin_drains_the_begin_writer_without_executing_tool() {
    let begin_gate = Arc::new(Notify::new());
    let log = Arc::new(Log {
        begin_gate: Some(begin_gate.clone()),
        ..Default::default()
    });
    let begin_log = log.clone();
    let parent_returned = Arc::new(Notify::new());
    let returned = parent_returned.clone();
    let effects = Arc::new(AtomicUsize::new(0));
    let actual = effects.clone();
    let execution = tokio::spawn(run(
        vec![
            tool(
                "program",
                Arc::new(move |_, _, _, _| {
                    let log = begin_log.clone();
                    let returned = returned.clone();
                    Box::pin(async move {
                        let p = current().unwrap();
                        let id = start(&p, "effect").await;
                        log.begin_entered.notified().await;
                        p.handle(json!({"op":"cancel","requestId":id}))
                            .await
                            .unwrap();
                        assert_eq!(poll(&p, &id).await["status"], "failed");
                        returned.notify_one();
                        Ok(AgentToolResult::text("root"))
                    })
                }),
            ),
            tool(
                "effect",
                Arc::new(move |_, _, _, _| {
                    actual.fetch_add(1, Ordering::SeqCst);
                    Box::pin(async { Ok(metadata(false)) })
                }),
            ),
        ],
        common::base_config(),
        log.clone(),
    ));
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        parent_returned.notified(),
    )
    .await
    .unwrap();
    assert!(!execution.is_finished());
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    assert!(!log.snapshot().iter().any(|e| e["phase"] == "closing"));
    begin_gate.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(3), execution)
        .await
        .unwrap()
        .unwrap();
    let events = log.snapshot();
    assert_eq!(events.iter().filter(|e| e["phase"] == "closed").count(), 1);
    assert!(!events
        .iter()
        .any(|e| e["phase"] == "prepared" || e["phase"] == "finish"));
    assert_eq!(effects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_and_original_deadline_remain_live_during_close() {
    for mode in ["cancel", "timeout"] {
        let close_gate = Arc::new(Notify::new());
        let log = Arc::new(Log {
            close_gate: Some(close_gate.clone()),
            ..Default::default()
        });
        let root_log = log.clone();
        let after_log = log.clone();
        let replied = Arc::new(Notify::new());
        let root_replied = replied.clone();
        let mut cfg = common::base_config();
        cfg.after_tool_call = Some(Arc::new(move |ctx, _| {
            if ctx.tool_call.name == "program" {
                after_log.event(json!({"phase":"root-after"}));
            }
            Box::pin(async { None })
        }));
        let execution = tokio::spawn(run(
            vec![
                tool(
                    "program",
                    Arc::new(move |_, _, _, _| {
                        let log = root_log.clone();
                        let replied = root_replied.clone();
                        Box::pin(async move {
                            let p = current().unwrap();
                            let id = p
                                .handle(json!({"op":"start","name":"effect","arguments":{},
                            "timeoutMs":if mode == "timeout" {1000} else {30000}}))
                                .await
                                .unwrap()["requestId"]
                                .clone();
                            log.close_entered.notified().await;
                            assert_eq!(
                                p.handle(json!({"op":"poll","requestId":id})).await.unwrap()
                                    ["status"],
                                "running"
                            );
                            if mode == "cancel" {
                                p.handle(json!({"op":"cancel","requestId":id}))
                                    .await
                                    .unwrap();
                            }
                            let reply = poll(&p, &id).await;
                            assert_eq!(reply["status"], "failed");
                            assert!(reply["error"].as_str().unwrap().contains(
                                if mode == "timeout" {
                                    "timed out"
                                } else {
                                    "cancelled"
                                }
                            ));
                            replied.notify_one();
                            Ok(AgentToolResult::text("root"))
                        })
                    }),
                ),
                tool(
                    "effect",
                    Arc::new(|_, _, _, _| Box::pin(async { Ok(metadata(false)) })),
                ),
            ],
            cfg,
            log.clone(),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(3), replied.notified())
            .await
            .unwrap();
        assert!(
            !execution.is_finished(),
            "root drain must retain active permits"
        );
        let before = log.snapshot();
        assert_eq!(before.iter().filter(|e| e["phase"] == "finish").count(), 1);
        assert_eq!(before.iter().filter(|e| e["phase"] == "closing").count(), 1);
        assert!(!before
            .iter()
            .any(|e| e["phase"] == "closed" || e["phase"] == "root-after"));
        close_gate.notify_one();
        let (_, result) = tokio::time::timeout(std::time::Duration::from_secs(3), execution)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.usage.unwrap().total_tokens, 3);
        assert_eq!(result.added_tool_names, vec!["native-added"]);
        let events = log.snapshot();
        assert_eq!(events.iter().filter(|e| e["phase"] == "closing").count(), 1);
        assert_eq!(events.iter().filter(|e| e["phase"] == "closed").count(), 1);
        assert!(
            events.iter().position(|e| e["phase"] == "closed").unwrap()
                < events
                    .iter()
                    .position(|e| e["phase"] == "root-after")
                    .unwrap()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_concurrent_update_retires_before_finish_and_close_even_when_cancelled() {
    for cancel in [false, true] {
        let latch = Arc::new((Mutex::new(false), Condvar::new()));
        let log = Arc::new(Log {
            update_gate: Some(latch.clone()),
            ..Default::default()
        });
        let root_log = log.clone();
        let native_log = log.clone();
        let retained = Arc::new(Notify::new());
        let root_retained = retained.clone();
        let released = Arc::new(Notify::new());
        let root_released = released.clone();
        let saved: Arc<Mutex<Option<Update>>> = Arc::new(Mutex::new(None));
        let native_saved = saved.clone();
        let threads = Arc::new(Mutex::new(Vec::new()));
        let native_threads = threads.clone();
        let execution = tokio::spawn(run(
            vec![
                tool(
                    "program",
                    Arc::new(move |_, _, _, _| {
                        let retained = root_retained.clone();
                        let released = root_released.clone();
                        let log = root_log.clone();
                        Box::pin(async move {
                            let p = current().unwrap();
                            let id = start(&p, "effect").await;
                            // Native terminate metadata is retained BEFORE callback retirement.
                            // Observing the admission latch proves execution has finalized;
                            // no scheduler sleeps or guessed callback timing are needed.
                            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                                while p.handle(json!({"op":"list"})).await.is_ok() {
                                    tokio::task::yield_now().await;
                                }
                            })
                            .await
                            .unwrap();
                            assert_eq!(
                                p.handle(json!({"op":"poll","requestId":id})).await.unwrap()
                                    ["status"],
                                "running"
                            );
                            assert!(!log
                                .snapshot()
                                .iter()
                                .any(|e| e["phase"] == "finish" || e["phase"] == "closing"));
                            if cancel {
                                p.handle(json!({"op":"cancel","requestId":id}))
                                    .await
                                    .unwrap();
                                assert_eq!(poll(&p, &id).await["status"], "failed");
                            }
                            retained.notify_one();
                            if !cancel {
                                released.notified().await;
                                assert_eq!(poll(&p, &id).await["status"], "done");
                            }
                            Ok(AgentToolResult::text("root"))
                        })
                    }),
                ),
                tool(
                    "effect",
                    Arc::new(move |_, _, _, update| {
                        let log = native_log.clone();
                        *native_saved.lock().unwrap() = Some(update.clone());
                        native_threads
                            .lock()
                            .unwrap()
                            .push(std::thread::spawn(move || update(signed())));
                        Box::pin(async move {
                            log.update_entered.notified().await;
                            Ok(metadata(true))
                        })
                    }),
                ),
            ],
            common::base_config(),
            log.clone(),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(3), retained.notified())
            .await
            .unwrap();
        assert!(!execution.is_finished());
        // An already accepted callback remains latched; a newly arriving callback
        // must not enter the writer at all (and thus must not hit that latch).
        saved.lock().unwrap().as_ref().unwrap()(signed());
        assert!(!log
            .snapshot()
            .iter()
            .any(|e| e["phase"] == "finish" || e["phase"] == "closing"));
        {
            let (ready, wake) = &*latch;
            *ready.lock().unwrap() = true;
            wake.notify_all();
        }
        released.notify_one();
        let (_, result) = tokio::time::timeout(std::time::Duration::from_secs(3), execution)
            .await
            .unwrap()
            .unwrap();
        for thread in threads.lock().unwrap().drain(..) {
            thread.join().unwrap();
        }
        assert_eq!(result.usage.unwrap().total_tokens, 3);
        assert_eq!(result.added_tool_names, vec!["native-added"]);
        let events = log.snapshot();
        assert_eq!(events.iter().filter(|e| e["phase"] == "update").count(), 1);
        let pos = |phase| events.iter().position(|e| e["phase"] == phase).unwrap();
        assert!(pos("update") < pos("finish"));
        assert!(pos("finish") < pos("closing"));
        assert_eq!(events.iter().filter(|e| e["phase"] == "closed").count(), 1);
        saved.lock().unwrap().as_ref().unwrap()(signed());
        assert_eq!(events, log.snapshot());
    }
}
