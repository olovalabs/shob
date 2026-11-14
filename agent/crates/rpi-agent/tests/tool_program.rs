//! Offline action-30 runtime safety and normal transcript isolation.
#[path = "common/mod.rs"]
mod common;
use futures::future::BoxFuture;
use rpi_agent::{
    tool_program::{current, ToolProgram},
    AfterToolCallResult, AgentContext, AgentError, AgentTool, AgentToolResult,
    BeforeToolCallResult, ToolExecutionMode, ToolResultPartial,
};
use rpi_ai::types::{StopReason, Tool};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio_util::sync::CancellationToken;
type Body = Arc<dyn Fn(Arc<ToolProgram>) -> BoxFuture<'static, ()> + Send + Sync>;
struct Program {
    schema: Tool,
    body: Body,
    mode: ToolExecutionMode,
}
fn schema(name: &str) -> Tool {
    serde_json::from_value(json!({"name":name,"description":"test tool","parameters":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}})).unwrap()
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
        self.mode
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        (self.body)(current().expect("native invocation")).await;
        Ok(AgentToolResult::text("program done"))
    }
}
struct Effect {
    schema: Tool,
    count: Arc<AtomicUsize>,
    delay: u64,
    sequential: bool,
}
#[async_trait::async_trait]
impl AgentTool for Effect {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "effect"
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        if self.sequential {
            ToolExecutionMode::Sequential
        } else {
            ToolExecutionMode::Parallel
        }
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(self.delay)).await;
        Ok(AgentToolResult::text("effect done"))
    }
}
async fn poll(p: &Arc<ToolProgram>, id: &Value) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let r = p.handle(json!({"op":"poll","requestId":id})).await.unwrap();
            if r["status"] != "running" {
                return r;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}
async fn start_result(p: &Arc<ToolProgram>) -> Result<Value, String> {
    p.handle(json!({"op":"start","name":"effect","arguments":{"value":"ok"}}))
        .await
}
async fn start(p: &Arc<ToolProgram>) -> Value {
    p.handle(json!({"op":"start","name":"effect","arguments":{"value":"ok"}}))
        .await
        .unwrap()["requestId"]
        .clone()
}
async fn run(body: Body, config: rpi_agent::AgentLoopConfig, count: Arc<AtomicUsize>, delay: u64) {
    let tools: Vec<Arc<dyn AgentTool>> = vec![
        Arc::new(Program {
            schema: schema("codemode"),
            body,
            mode: ToolExecutionMode::Sequential,
        }),
        Arc::new(Effect {
            schema: schema("effect"),
            count,
            delay,
            sequential: true,
        }),
    ];
    run_tools(tools, config).await;
}
async fn run_tools(tools: Vec<Arc<dyn AgentTool>>, config: rpi_agent::AgentLoopConfig) {
    let context = AgentContext {
        tools,
        ..Default::default()
    };
    let (events, messages) = common::run_and_collect(
        vec![common::user_message("run")],
        context,
        config,
        common::mock_stream_fn(vec![
            common::assistant_tool_calls(
                vec![("parent", "codemode", json!({"value":"body"}))],
                StopReason::ToolUse,
            ),
            common::assistant_text("done", StopReason::Stop),
        ]),
    )
    .await;
    assert!(
        messages
            .iter()
            .all(|m| !matches!(m,rpi_agent::AgentMessage::ToolResult(r) if r.is_error)),
        "outer tool failed: {messages:?}"
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m, rpi_agent::AgentMessage::ToolResult(_)))
            .count(),
        1,
        "nested calls must not become transcript messages"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, rpi_agent::AgentEvent::ToolExecutionStart { .. }))
            .count(),
        1,
        "no nested ordinary events"
    );
}
#[tokio::test]
async fn catalog_subset_recursion_validation_hooks_and_finalized_result() {
    let count = Arc::new(AtomicUsize::new(0));
    let mut cfg = common::base_config();
    cfg.before_tool_call = Some(Arc::new(|ctx, _| {
        let block = ctx.tool_call.name == "effect" && ctx.args["value"] == "blocked";
        Box::pin(async move {
            block.then(|| BeforeToolCallResult {
                block: true,
                reason: Some("approval denied".into()),
                ..Default::default()
            })
        })
    }));
    cfg.after_tool_call = Some(Arc::new(|ctx, _| {
        let effect = ctx.tool_call.name == "effect" && !ctx.is_error;
        Box::pin(async move {
            effect.then(|| AfterToolCallResult {
                details: Some(json!({"finalized":true})),
                content: Some(vec![rpi_agent::TextContentOrImage::Image(
                    serde_json::from_value(json!({"type":"image", "data":"AA==", "mimeType":"image/png"})).unwrap()
                )]),
                terminate: Some(true),
                usage: Some(serde_json::from_value(json!({
                    "input":1, "output":2, "cacheRead":0, "cacheWrite":0, "totalTokens":3,
                    "cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}
                })).unwrap()),
                ..Default::default()
            })
        })
    }));
    run(
        Arc::new(|p| {
            Box::pin(async move {
                let page = p.handle(json!({"op":"list","limit":1})).await.unwrap();
                assert_eq!(page["protocolVersion"], 1);
                assert_eq!(page["tools"][0]["name"], "effect");
                assert!(page["tools"][0]["parameters"].is_object());
                assert!(page["nextOffset"].is_null());
                for name in ["codemode", "root-secret", "__rpiRuntimeKey"] {
                    assert!(p
                        .handle(json!({"op":"start","name":name,"arguments":{}}))
                        .await
                        .is_err());
                }
                assert!(p
                    .handle(json!({"op":"start","name":"effect","arguments":[]}))
                    .await
                    .is_err());
                for arguments in [json!({}), json!({"value":"blocked"})] {
                    let id = p
                        .handle(json!({"op":"start","name":"effect","arguments":arguments}))
                        .await
                        .unwrap()["requestId"]
                        .clone();
                    let reply = poll(&p, &id).await;
                    assert_eq!(reply["status"], "done");
                    assert_eq!(reply["isError"], true);
                }
                let id = start(&p).await;
                let reply = poll(&p, &id).await;
                assert_eq!(reply["isError"], false);
                assert_eq!(reply["result"]["details"]["finalized"], true);
                assert_eq!(reply["result"]["content"][0]["type"], "image");
                assert_eq!(reply["result"]["content"][0]["data"], "AA==");
                assert_eq!(reply["result"]["usage"]["totalTokens"], 3);
                assert_eq!(reply["result"]["terminate"], true);
                assert!(start_result(&p).await.is_err());
            })
        }),
        cfg,
        count.clone(),
        0,
    )
    .await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn sequential_parent_does_not_deadlock_parallel_requests_and_stale_capability_closes() {
    let saved = Arc::new(Mutex::new(None));
    let out = saved.clone();
    let mut cfg = common::base_config();
    cfg.tool_execution = ToolExecutionMode::Sequential;
    let count = Arc::new(AtomicUsize::new(0));
    run(
        Arc::new(move |p| {
            let out = out.clone();
            Box::pin(async move {
                let a = start(&p).await;
                let b = start(&p).await;
                assert_ne!(a, b);
                assert_eq!(poll(&p, &a).await["status"], "done");
                assert_eq!(poll(&p, &b).await["status"], "done");
                *out.lock().unwrap() = Some(p);
            })
        }),
        cfg,
        count.clone(),
        2,
    )
    .await;
    let p = saved.lock().unwrap().take().unwrap();
    assert!(p.handle(json!({"op":"list"})).await.is_err());
    assert!(p
        .handle(json!({"op":"start","name":"effect","arguments":{}}))
        .await
        .is_err());
    assert_eq!(count.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn cancellation_timeout_retention_active_limits_and_cleanup() {
    run(
        Arc::new(|p| {
            Box::pin(async move {
                let first = p.handle(json!({
                "op": "start", "name": "effect", "arguments": {"value": "ok"}, "timeoutMs": 1
            })).await.unwrap()["requestId"].clone();
                assert_eq!(poll(&p, &first).await["status"], "failed");
                // A timed-out noncooperative native future still owns its permit.
                p.handle(json!({"op":"dispose", "requestId": first}))
                    .await
                    .unwrap();
                let mut ids = Vec::new();
                for _ in 0..7 {
                    ids.push(start(&p).await);
                }
                assert!(p
                    .handle(json!({
                        "op":"start", "name":"effect", "arguments":{"value":"ok"}
                    }))
                    .await
                    .unwrap_err()
                    .contains("limit"));
                p.cancel_all();
                assert!(p.handle(json!({"op":"list"})).await.is_err());
                for id in ids {
                    assert_eq!(poll(&p, &id).await["status"], "failed");
                    for _ in 0..2 {
                        p.handle(json!({"op":"dispose", "requestId": id}))
                            .await
                            .unwrap();
                    }
                }
                assert!(p
                    .handle(json!({"op":"poll", "requestId":"forged"}))
                    .await
                    .is_err());
            })
        }),
        common::base_config(),
        Arc::new(AtomicUsize::new(0)),
        200,
    )
    .await;
}

#[tokio::test]
async fn timed_out_future_retains_sequential_gate_through_cleanup() {
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    run(
        Arc::new(move |p| {
            let observed = observed.clone();
            Box::pin(async move {
                let first = p
                    .handle(json!({
                        "op":"start", "name":"effect", "arguments":{"value":"ok"}, "timeoutMs":10
                    }))
                    .await
                    .unwrap()["requestId"]
                    .clone();
                assert_eq!(poll(&p, &first).await["status"], "failed");
                assert_eq!(observed.load(Ordering::SeqCst), 1);
                let second = start(&p).await;
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                assert_eq!(
                    observed.load(Ordering::SeqCst),
                    1,
                    "cleanup still owns exclusive gate"
                );
                assert_eq!(poll(&p, &second).await["status"], "done");
            })
        }),
        common::base_config(),
        count.clone(),
        100,
    )
    .await;
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn total_starts_budget_and_argument_bounds() {
    run(
        Arc::new(|p| {
            Box::pin(async move {
                assert!(p
                    .handle(json!({
                        "op":"start", "name":"effect", "arguments":{"value":"x".repeat(1024*1024)}
                    }))
                    .await
                    .is_err());
                for _ in 0..128 {
                    let id = start(&p).await;
                    assert_eq!(poll(&p, &id).await["status"], "done");
                    p.handle(json!({"op":"dispose", "requestId":id}))
                        .await
                        .unwrap();
                }
                assert!(p
                    .handle(json!({
                        "op":"start", "name":"effect", "arguments":{"value":"ok"}
                    }))
                    .await
                    .unwrap_err()
                    .contains("budget"));
            })
        }),
        common::base_config(),
        Arc::new(AtomicUsize::new(0)),
        0,
    )
    .await;
}

#[tokio::test]
async fn parallel_program_admission_fails_closed() {
    run_tools(
        vec![Arc::new(Program {
            schema: schema("codemode"),
            mode: ToolExecutionMode::Parallel,
            body: Arc::new(|p| {
                Box::pin(async move {
                    assert!(p.handle(json!({"op":"list"})).await.is_ok());
                    let error = p
                        .handle(json!({"op":"start", "name":"effect", "arguments":{}}))
                        .await
                        .unwrap_err();
                    assert!(error.contains("Sequential"));
                })
            }),
        })],
        common::base_config(),
    )
    .await;
}

#[tokio::test]
async fn nested_programs_share_active_budget_and_preserve_child_scope() {
    let child = Arc::new(
        rpi_agent::tool_execution_scope::ToolExecutionScope::with_context(
            json!({"tools":["codemode", "nested", "effect"]}),
        ),
    );
    let tools: Vec<Arc<dyn AgentTool>> = vec![
        Arc::new(Program {
            schema: schema("codemode"),
            mode: ToolExecutionMode::Sequential,
            body: Arc::new(|p| {
                Box::pin(async move {
                    let id = p
                        .handle(json!({"op":"start", "name":"nested", "arguments":{"value":"ok"}}))
                        .await
                        .unwrap()["requestId"]
                        .clone();
                    assert_eq!(poll(&p, &id).await["isError"], false);
                })
            }),
        }),
        Arc::new(Program {
            schema: schema("nested"),
            mode: ToolExecutionMode::Sequential,
            body: Arc::new(|p| {
                Box::pin(async move {
                    assert!(rpi_agent::tool_execution_scope::is_child_execution());
                    assert_eq!(
                        rpi_agent::tool_execution_scope::execution_context().unwrap()["tools"],
                        json!(["codemode", "nested", "effect"])
                    );
                    let catalog = p.handle(json!({"op":"list"})).await.unwrap();
                    assert_eq!(catalog["tools"].as_array().unwrap().len(), 1);
                    assert_eq!(catalog["tools"][0]["name"], "effect");
                    let mut ids = Vec::new();
                    for _ in 0..7 {
                        ids.push(start(&p).await);
                    }
                    assert!(start_result(&p)
                        .await
                        .unwrap_err()
                        .contains("invocation-tree"));
                    for id in ids {
                        assert_eq!(poll(&p, &id).await["status"], "done");
                    }
                })
            }),
        }),
        Arc::new(Effect {
            schema: schema("effect"),
            count: Arc::new(AtomicUsize::new(0)),
            delay: 0,
            sequential: true,
        }),
    ];
    child.run(run_tools(tools, common::base_config())).await;
    child.wait_idle().await;
}

#[tokio::test]
async fn driver_signal_cancels_already_started_native_work() {
    run(
        Arc::new(|p| {
            Box::pin(async move {
                let driver = CancellationToken::new();
                p.bind_signal(driver.clone());
                let id = start(&p).await;
                driver.cancel();
                assert_eq!(poll(&p, &id).await["status"], "failed");
                assert!(start_result(&p).await.is_err());
            })
        }),
        common::base_config(),
        Arc::new(AtomicUsize::new(0)),
        10,
    )
    .await;
}

#[tokio::test]
async fn outer_program_waits_for_timed_out_native_cleanup_before_returning() {
    let began = std::time::Instant::now();
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    run(Arc::new(move |p| {
        let observed = observed.clone();
        Box::pin(async move {
            let id = p.handle(json!({"op":"start", "name":"effect", "arguments":{"value":"ok"}, "timeoutMs":10}))
                .await.unwrap()["requestId"].clone();
            assert_eq!(poll(&p, &id).await["status"], "failed");
            assert_eq!(observed.load(Ordering::SeqCst), 1);
            // Return without waiting for the cancelled native effect.
        })
    }), common::base_config(), count, 100).await;
    assert!(began.elapsed() >= std::time::Duration::from_millis(100));
}

/// A native descendant that observes cancellation but keeps its cleanup alive.
struct CleanupProbe {
    schema: Tool,
    live: Arc<AtomicUsize>,
    cleaned: Arc<AtomicUsize>,
    overlap: Arc<AtomicUsize>,
    observe: bool,
}
#[async_trait::async_trait]
impl AgentTool for CleanupProbe {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "cleanup probe"
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        signal: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        if self.observe {
            self.overlap
                .fetch_add(self.live.load(Ordering::SeqCst), Ordering::SeqCst);
        } else {
            self.live.store(1, Ordering::SeqCst);
            signal.cancelled().await;
            // Cancellation cannot interrupt a foreign/native destroy callback.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            self.live.store(0, Ordering::SeqCst);
            self.cleaned.fetch_add(1, Ordering::SeqCst);
        }
        Ok(AgentToolResult::text("cleaned"))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panicking_nested_program_drains_descendants_before_releasing_its_gate() {
    // Check both root return and a later sequential sibling after polling failure.
    for sibling in [false, true] {
        let live = Arc::new(AtomicUsize::new(0));
        let cleaned = Arc::new(AtomicUsize::new(0));
        let overlap = Arc::new(AtomicUsize::new(0));
        let before = Arc::new(AtomicUsize::new(0));
        let after = Arc::new(AtomicUsize::new(0));
        let mut cfg = common::base_config();
        let seen = before.clone();
        cfg.before_tool_call = Some(Arc::new(move |ctx, _| {
            if ctx.tool_call.name == "nested" {
                seen.fetch_add(1, Ordering::SeqCst);
            }
            Box::pin(async { None })
        }));
        let seen = after.clone();
        cfg.after_tool_call = Some(Arc::new(move |ctx, _| {
            if ctx.tool_call.name == "nested" {
                seen.fetch_add(1, Ordering::SeqCst);
            }
            Box::pin(async { None })
        }));
        let observed = live.clone();
        let tools: Vec<Arc<dyn AgentTool>> = vec![
            Arc::new(Program {
                schema: schema("codemode"),
                mode: ToolExecutionMode::Sequential,
                body: Arc::new(move |p| {
                    Box::pin(async move {
                        let id = p
                            .handle(json!({
                                "op":"start", "name":"nested", "arguments":{"value":"ok"}
                            }))
                            .await
                            .unwrap()["requestId"]
                            .clone();
                        let failure = poll(&p, &id).await;
                        assert_eq!(failure["status"], "failed");
                        assert_eq!(failure["error"], "nested invocation panicked");
                        if sibling {
                            let id = p
                                .handle(json!({
                                    "op":"start", "name":"probe", "arguments":{"value":"ok"}
                                }))
                                .await
                                .unwrap()["requestId"]
                                .clone();
                            assert_eq!(poll(&p, &id).await["status"], "done");
                        }
                    })
                }),
            }),
            Arc::new(Program {
                schema: schema("nested"),
                mode: ToolExecutionMode::Sequential,
                body: Arc::new(move |p| {
                    let observed = observed.clone();
                    Box::pin(async move {
                        p.handle(json!({
                            "op":"start", "name":"slow", "arguments":{"value":"ok"}
                        }))
                        .await
                        .unwrap();
                        tokio::time::timeout(std::time::Duration::from_secs(1), async {
                            while observed.load(Ordering::SeqCst) == 0 {
                                tokio::task::yield_now().await;
                            }
                        })
                        .await
                        .unwrap();
                        panic!("controlled nested program panic");
                    })
                }),
            }),
            Arc::new(CleanupProbe {
                schema: schema("slow"),
                live: live.clone(),
                cleaned: cleaned.clone(),
                overlap: overlap.clone(),
                observe: false,
            }),
            Arc::new(CleanupProbe {
                schema: schema("probe"),
                live: live.clone(),
                cleaned: cleaned.clone(),
                overlap: overlap.clone(),
                observe: true,
            }),
        ];
        tokio::time::timeout(std::time::Duration::from_secs(3), run_tools(tools, cfg))
            .await
            .unwrap();
        assert_eq!(
            overlap.load(Ordering::SeqCst),
            0,
            "sibling overlapped cleanup"
        );
        assert_eq!(
            live.load(Ordering::SeqCst),
            0,
            "root left a live descendant"
        );
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        assert_eq!(before.load(Ordering::SeqCst), 1);
        // A panic is still a lifecycle failure, not a finalized tool error.
        assert_eq!(after.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn oversized_finalized_terminate_latches_before_reply_encoding() {
    // Admission follows the final hook override, including an oversized error
    // result, not the original tool result or whether encoding succeeds.
    for (terminate, is_error) in [(true, false), (true, true), (false, false)] {
        let count = Arc::new(AtomicUsize::new(0));
        let mut cfg = common::base_config();
        cfg.after_tool_call = Some(Arc::new(move |ctx, _| {
            let effect = ctx.tool_call.name == "effect";
            Box::pin(async move {
                effect.then(|| AfterToolCallResult {
                    terminate: Some(terminate),
                    is_error: Some(is_error),
                    details: Some(json!({"large":"x".repeat(1024 * 1024)})),
                    ..Default::default()
                })
            })
        }));
        run(
            Arc::new(move |p| {
                Box::pin(async move {
                    let id = start(&p).await;
                    let reply = poll(&p, &id).await;
                    assert_eq!(reply["status"], "failed");
                    assert_eq!(reply["error"], "ToolProgram JSON exceeds 1 MiB");
                    if terminate {
                        assert_eq!(
                            start_result(&p).await.unwrap_err(),
                            "parent invocation is no longer active"
                        );
                        assert!(p.handle(json!({"op":"list"})).await.is_err());
                    } else {
                        let id = start(&p).await;
                        assert_eq!(poll(&p, &id).await["status"], "failed");
                    }
                })
            }),
            cfg,
            count.clone(),
            0,
        )
        .await;
        assert_eq!(count.load(Ordering::SeqCst), if terminate { 1 } else { 2 });
    }
}

// Native parity follow-up: host rollup must not depend on guest result handling.
type Update = Arc<dyn Fn(ToolResultPartial) + Send + Sync>;
type ParityBody = Arc<
    dyn Fn(
            Arc<ToolProgram>,
            CancellationToken,
            Update,
        ) -> BoxFuture<'static, Result<AgentToolResult, AgentError>>
        + Send
        + Sync,
>;
struct ParityTool {
    schema: Tool,
    body: ParityBody,
}
#[async_trait::async_trait]
impl AgentTool for ParityTool {
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
        _: &str,
        _: Value,
        signal: CancellationToken,
        update: Update,
    ) -> Result<AgentToolResult, AgentError> {
        (self.body)(current().unwrap(), signal, update).await
    }
}
fn parity_tool(name: &str, body: ParityBody) -> Arc<dyn AgentTool> {
    Arc::new(ParityTool {
        schema: schema(name),
        body,
    })
}
fn metadata(tokens: i64, terminate: bool) -> AgentToolResult {
    let mut usage = rpi_ai::types::Usage::zero();
    usage.input = tokens;
    usage.total_tokens = tokens;
    usage.reasoning = Some(tokens);
    usage.cache_write_1h = Some(tokens);
    usage.cost.total = tokens as f64 / 10.0;
    AgentToolResult {
        usage: Some(usage),
        added_tool_names: vec!["z".into(), "a".into(), "z".into()],
        terminate,
        ..AgentToolResult::text("native")
    }
}
async fn parity_run(
    tools: Vec<Arc<dyn AgentTool>>,
    cfg: rpi_agent::AgentLoopConfig,
) -> (Vec<rpi_agent::AgentEvent>, rpi_ai::types::ToolResultMessage) {
    let (events, messages) = common::run_and_collect(
        vec![common::user_message("run")],
        AgentContext {
            tools,
            ..Default::default()
        },
        cfg,
        common::mock_stream_fn(vec![
            common::assistant_tool_calls(
                vec![("parent", "codemode", json!({"value":"private-args"}))],
                StopReason::ToolUse,
            ),
            common::assistant_text("done", StopReason::Stop),
        ]),
    )
    .await;
    let results: Vec<_> = messages
        .into_iter()
        .filter_map(|m| match m {
            rpi_agent::AgentMessage::ToolResult(r) => Some(*r),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1, "no synthetic nested transcript results");
    (events, results.into_iter().next().unwrap())
}

#[tokio::test]
async fn host_rollup_survives_errors_repeated_poll_disposal_and_unawaited_drain() {
    for failure in [0, 1, 2] {
        let finished = Arc::new(AtomicUsize::new(0));
        let done = finished.clone();
        let tools = vec![
            parity_tool(
                "codemode",
                Arc::new(move |p, _, _| {
                    Box::pin(async move {
                        let page = p.handle(json!({"op":"list"})).await.unwrap();
                        assert_eq!(
                            page["capabilities"],
                            json!({"resultEnvelope":1,"hostAccounting":1,"parentProgress":1,"losslessValues":1,"resultReferences":1,"contentAttachments":1})
                        );
                        let id = start(&p).await;
                        for _ in 0..3 {
                            assert_eq!(poll(&p, &id).await["status"], "done");
                        }
                        p.handle(json!({"op":"dispose", "requestId":id}))
                            .await
                            .unwrap();
                        let id = start(&p).await;
                        // Dispose an executing call, but retain its eventual finalized metadata.
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        p.handle(json!({"op":"dispose", "requestId":id}))
                            .await
                            .unwrap();
                        match failure {
                            0 => Ok(metadata(7, false)),
                            1 => Err(AgentError::tool("ordinary script failure")),
                            _ => Err(AgentError::StructuredTool(Box::new(metadata(7, false)))),
                        }
                    })
                }),
            ),
            parity_tool(
                "effect",
                Arc::new(move |_, _, _| {
                    let done = done.clone();
                    Box::pin(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                        done.fetch_add(1, Ordering::SeqCst);
                        Ok(metadata(3, false))
                    })
                }),
            ),
        ];
        let (_, result) = parity_run(tools, common::base_config()).await;
        assert_eq!(finished.load(Ordering::SeqCst), 2);
        assert_eq!(result.is_error, failure != 0);
        let usage = result.usage.unwrap();
        assert_eq!(usage.total_tokens, if failure == 1 { 6 } else { 13 });
        assert_eq!(usage.reasoning, Some(usage.total_tokens));
        assert_eq!(usage.cache_write_1h, Some(usage.total_tokens));
        assert!((usage.cost.total - usage.total_tokens as f64 / 10.0).abs() < 0.00001);
        assert_eq!(result.added_tool_names, vec!["a", "z"]);
    }
}

#[tokio::test]
async fn host_rollup_nested_programs_counts_each_direct_child_once() {
    let (_, result) = parity_run(
        vec![
            parity_tool(
                "codemode",
                Arc::new(|p, _, _| {
                    Box::pin(async move {
                        let id = p
                            .handle(
                                json!({"op":"start","name":"nested","arguments":{"value":"ok"}}),
                            )
                            .await
                            .unwrap()["requestId"]
                            .clone();
                        assert_eq!(poll(&p, &id).await["result"]["usage"]["totalTokens"], 5);
                        Ok(metadata(7, false))
                    })
                }),
            ),
            parity_tool(
                "nested",
                Arc::new(|p, _, _| {
                    Box::pin(async move {
                        let id = start(&p).await;
                        assert_eq!(poll(&p, &id).await["status"], "done");
                        Ok(metadata(2, false))
                    })
                }),
            ),
            parity_tool(
                "effect",
                Arc::new(|_, _, _| Box::pin(async { Ok(metadata(3, false)) })),
            ),
        ],
        common::base_config(),
    )
    .await;
    assert_eq!(result.usage.unwrap().total_tokens, 12);
    assert_eq!(result.added_tool_names, vec!["a", "z"]);
}

#[tokio::test]
async fn cancelled_and_oversized_child_metadata_is_available_before_outer_after_hook() {
    for oversized in [false, true] {
        let seen = Arc::new(AtomicUsize::new(0));
        let observed = seen.clone();
        let mut cfg = common::base_config();
        cfg.after_tool_call = Some(Arc::new(move |ctx, _| {
            if ctx.tool_call.name == "codemode" {
                assert!(ctx.result.terminate);
                assert_eq!(ctx.result.usage.as_ref().unwrap().total_tokens, 3);
                assert_eq!(ctx.result.added_tool_names, vec!["a", "z"]);
                assert!(ctx.is_error);
                observed.fetch_add(1, Ordering::SeqCst);
                // Ordinary native after hooks retain final authority.
                return Box::pin(async {
                    Some(AfterToolCallResult {
                        terminate: Some(false),
                        is_error: Some(false),
                        usage: Some(rpi_ai::types::Usage::zero()),
                        ..Default::default()
                    })
                });
            }
            Box::pin(async { None })
        }));
        let (_, result) = parity_run(vec![
            parity_tool("codemode", Arc::new(move |p, _, _| Box::pin(async move {
                let id = p.handle(json!({"op":"start","name":"effect","arguments":{"value":"ok"},"timeoutMs":if oversized {1000} else {5}})).await.unwrap()["requestId"].clone();
                assert_eq!(poll(&p, &id).await["status"], "failed");
                Err(AgentError::tool("script failed after child"))
            }))),
            parity_tool("effect", Arc::new(move |_, _, _| Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                let mut result = metadata(3, true);
                if oversized { result.details = json!({"large":"x".repeat(1024*1024)}); }
                Err(AgentError::StructuredTool(Box::new(result)))
            }))),
        ], cfg).await;
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        assert!(!result.is_error);
        assert_eq!(result.usage.unwrap().total_tokens, 0);
    }
}

#[tokio::test]
async fn structured_native_error_preserves_result_before_authoritative_hooks() {
    let mut cfg = common::base_config();
    cfg.after_tool_call = Some(Arc::new(|ctx, _| {
        assert!(ctx.is_error);
        assert_eq!(ctx.result.details, json!({"bounded":"summary"}));
        assert!(matches!(
            ctx.result.content[0],
            rpi_agent::TextContentOrImage::Image(_)
        ));
        assert_eq!(ctx.result.usage.as_ref().unwrap().total_tokens, 4);
        assert!(ctx.result.terminate);
        Box::pin(async {
            Some(AfterToolCallResult {
                is_error: Some(false),
                content: Some(vec![rpi_agent::TextContentOrImage::text("hook")]),
                ..Default::default()
            })
        })
    }));
    let (_, result) = parity_run(
        vec![parity_tool(
            "codemode",
            Arc::new(|_, _, _| {
                Box::pin(async {
                    let mut result = metadata(4, true);
                    result.details = json!({"bounded":"summary"});
                    result.content = vec![rpi_agent::TextContentOrImage::Image(
                        serde_json::from_value(
                            json!({"type":"image","data":"AA==","mimeType":"image/png"}),
                        )
                        .unwrap(),
                    )];
                    Err(AgentError::StructuredTool(Box::new(result)))
                })
            }),
        )],
        cfg,
    )
    .await;
    assert!(!result.is_error);
    assert_eq!(result.content[0], rpi_ai::types::Content::text("hook"));
}

#[tokio::test]
async fn nested_progress_uses_outer_callback_and_only_bounded_lifecycle_metadata() {
    let (_, result) = parity_run(
        vec![parity_tool(
            "codemode",
            Arc::new(|_, _, _| Box::pin(async { Ok(AgentToolResult::text("baseline")) })),
        )],
        common::base_config(),
    )
    .await;
    assert!(!result.is_error);
    let (events, _) = parity_run(vec![
        parity_tool("codemode", Arc::new(|p, _, _| Box::pin(async move {
            let id = p.handle(json!({"op":"start","name":"nested","arguments":{"value":"nested-secret"}})).await.unwrap()["requestId"].clone();
            assert_eq!(poll(&p, &id).await["status"], "done");
            Ok(AgentToolResult::text("done"))
        }))),
        parity_tool("nested", Arc::new(|p, _, _| Box::pin(async move {
            let id = start(&p).await;
            assert_eq!(poll(&p, &id).await["status"], "done");
            Ok(AgentToolResult::text("done"))
        }))),
        parity_tool("effect", Arc::new(|_, _, update| Box::pin(async move {
            for _ in 0..3 {
                update(AgentToolResult { details: json!({"runtimeKey":"secret-capability","image":"raw-image-secret"}), ..AgentToolResult::text("raw-payload-secret") });
                tokio::task::yield_now().await;
            }
            Ok(AgentToolResult::text("done"))
        }))),
    ], common::base_config()).await;
    let updates: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            rpi_agent::AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                partial_result,
                ..
            } => {
                assert_eq!(tool_call_id, "parent");
                assert_eq!(tool_name, "codemode");
                assert!(partial_result.content.is_empty());
                let text = partial_result.details.to_string();
                for secret in [
                    "runtimeKey",
                    "secret-capability",
                    "raw-image-secret",
                    "raw-payload-secret",
                    "nested-secret",
                ] {
                    assert!(!text.contains(secret));
                }
                Some((i, partial_result.details["toolProgram"].clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(updates.len(), 7); // Two lifecycle pairs plus three child updates.
    let nested_id = updates
        .iter()
        .find(|(_, d)| d["tool"] == "nested")
        .unwrap()
        .1["requestId"]
        .clone();
    for (_, d) in &updates {
        assert_eq!(d["rootId"], "parent");
        assert_eq!(
            d["parentId"],
            if d["tool"] == "nested" {
                json!("parent")
            } else {
                nested_id.clone()
            }
        );
    }
    let completed = updates
        .iter()
        .find(|(_, d)| d["tool"] == "effect" && d["phase"] == "completed")
        .unwrap();
    assert_eq!(completed.1["updateCount"], 3);
    let end = events
        .iter()
        .position(|e| matches!(e, rpi_agent::AgentEvent::ToolExecutionEnd { .. }))
        .unwrap();
    assert!(updates.iter().all(|(i, _)| *i < end));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, rpi_agent::AgentEvent::ToolExecutionStart { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn progress_budget_is_tree_wide_and_cancellation_and_late_updates_are_suppressed() {
    for cancel in [false, true] {
        let saved: Arc<Mutex<Option<Update>>> = Arc::new(Mutex::new(None));
        let out = saved.clone();
        let mut cfg = common::base_config();
        cfg.after_tool_call = Some(Arc::new(move |ctx, _| {
            if ctx.tool_call.name == "codemode" {
                // Child and root update gates have closed by after-hook execution.
                out.lock().unwrap().as_ref().unwrap()(AgentToolResult::text("late-secret"));
            }
            Box::pin(async { None })
        }));
        let (events, _) = parity_run(
            vec![
                parity_tool(
                    "codemode",
                    Arc::new(move |p, _, _| {
                        Box::pin(async move {
                            let id = start(&p).await;
                            assert_eq!(poll(&p, &id).await["status"], "done");
                            Ok(AgentToolResult::text("done"))
                        })
                    }),
                ),
                parity_tool(
                    "effect",
                    Arc::new(move |_, signal, update| {
                        let saved = saved.clone();
                        Box::pin(async move {
                            *saved.lock().unwrap() = Some(update.clone());
                            if cancel {
                                signal.cancel();
                            }
                            for _ in 0..600 {
                                update(AgentToolResult::text("secret"));
                            }
                            Ok(AgentToolResult::text("done"))
                        })
                    }),
                ),
            ],
            cfg,
        )
        .await;
        let projected: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                rpi_agent::AgentEvent::ToolExecutionUpdate { partial_result, .. } => {
                    Some(partial_result)
                }
                _ => None,
            })
            .collect();
        assert!(!projected.is_empty());
        assert!(projected.len() <= 256);
        assert!(
            projected
                .iter()
                .map(|p| p.details.to_string().len())
                .sum::<usize>()
                <= 64 * 1024
        );
        if cancel {
            assert_eq!(
                projected.len(),
                2,
                "self-cancel suppresses raw progress; request still finalizes its lifecycle"
            );
        }
    }
}

#[tokio::test]
async fn root_rust_panic_drains_native_descendants_before_unwinding_and_skips_after_hook() {
    use futures::FutureExt;
    let live = Arc::new(AtomicUsize::new(0));
    let cleaned = Arc::new(AtomicUsize::new(0));
    let hooks = Arc::new(AtomicUsize::new(0));
    let seen = live.clone();
    let observed = hooks.clone();
    let mut cfg = common::base_config();
    cfg.after_tool_call = Some(Arc::new(move |ctx, _| {
        if ctx.tool_call.name == "codemode" {
            observed.fetch_add(1, Ordering::SeqCst);
        }
        Box::pin(async { None })
    }));
    let tools: Vec<Arc<dyn AgentTool>> = vec![
        parity_tool(
            "codemode",
            Arc::new(move |p, _, _| {
                let seen = seen.clone();
                Box::pin(async move {
                    p.handle(json!({"op":"start","name":"slow","arguments":{"value":"ok"}}))
                        .await
                        .unwrap();
                    while seen.load(Ordering::SeqCst) == 0 {
                        tokio::task::yield_now().await;
                    }
                    panic!("controlled root panic");
                })
            }),
        ),
        Arc::new(CleanupProbe {
            schema: schema("slow"),
            live: live.clone(),
            cleaned: cleaned.clone(),
            overlap: Arc::new(AtomicUsize::new(0)),
            observe: false,
        }),
    ];
    let result = std::panic::AssertUnwindSafe(parity_run(tools, cfg))
        .catch_unwind()
        .await;
    assert!(result.is_err());
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert_eq!(cleaned.load(Ordering::SeqCst), 1);
    assert_eq!(hooks.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn checked_rollup_overflow_returns_native_error_not_panic() {
    let (_, result) = parity_run(
        vec![
            parity_tool(
                "codemode",
                Arc::new(|p, _, _| {
                    Box::pin(async move {
                        let id = start(&p).await;
                        assert_eq!(poll(&p, &id).await["status"], "done");
                        Ok(metadata(1, false))
                    })
                }),
            ),
            parity_tool(
                "effect",
                Arc::new(|_, _, _| Box::pin(async { Ok(metadata(i64::MAX, false)) })),
            ),
        ],
        common::base_config(),
    )
    .await;
    assert!(result.is_error);
    assert!(result.usage.is_none());
    assert!(common::tool_result_text(&result).contains("accounting overflow"));
    assert_eq!(result.added_tool_names, vec!["a", "z"]);
}

#[tokio::test]
async fn parent_cancellation_suppresses_cleanup_progress_and_retains_finalized_usage() {
    let ready = Arc::new(AtomicUsize::new(0));
    let started = ready.clone();
    let mut cfg = common::base_config();
    let observed = ready.clone();
    cfg.after_tool_call = Some(Arc::new(move |ctx, _| {
        if ctx.tool_call.name == "codemode" {
            assert_eq!(ctx.result.usage.as_ref().unwrap().total_tokens, 3);
            assert!(ctx.result.terminate);
            assert_eq!(observed.load(Ordering::SeqCst), 2);
        }
        Box::pin(async { None })
    }));
    let (events, result) = parity_run(
        vec![
            parity_tool(
                "codemode",
                Arc::new(move |p, _, _| {
                    let ready = ready.clone();
                    Box::pin(async move {
                        let id = start(&p).await;
                        while ready.load(Ordering::SeqCst) == 0 {
                            tokio::task::yield_now().await;
                        }
                        p.cancel_all();
                        assert_eq!(poll(&p, &id).await["status"], "failed");
                        Err(AgentError::tool("cancelled parent"))
                    })
                }),
            ),
            parity_tool(
                "effect",
                Arc::new(move |_, signal, update| {
                    let started = started.clone();
                    Box::pin(async move {
                        update(AgentToolResult::text("before cancellation"));
                        started.store(1, Ordering::SeqCst);
                        signal.cancelled().await;
                        update(AgentToolResult::text("cancelled secret"));
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        started.store(2, Ordering::SeqCst);
                        Ok(metadata(3, true))
                    })
                }),
            ),
        ],
        cfg,
    )
    .await;
    assert!(result.is_error);
    assert_eq!(result.usage.unwrap().total_tokens, 3);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, rpi_agent::AgentEvent::ToolExecutionUpdate { .. }))
            .count(),
        2
    );
}

struct PanickingProgressEmitter;
impl rpi_agent::AgentEmitter for PanickingProgressEmitter {
    fn emit(&self, _: rpi_agent::AgentEvent) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
    fn try_emit(&self, _: rpi_agent::AgentEvent) {
        panic!("observer panic must not bypass drain");
    }
}
#[tokio::test]
async fn panicking_progress_observer_cannot_drop_timed_out_native_cleanup() {
    let cleaned = Arc::new(AtomicUsize::new(0));
    let done = cleaned.clone();
    let tools = vec![
        parity_tool(
            "codemode",
            Arc::new(|p, _, _| {
                Box::pin(async move {
                    let id = p.handle(json!({"op":"start","name":"effect","arguments":{"value":"ok"},"timeoutMs":5})).await.unwrap()["requestId"].clone();
                    assert_eq!(poll(&p, &id).await["status"], "failed");
                    Ok(AgentToolResult::text("done"))
                })
            }),
        ),
        parity_tool(
            "effect",
            Arc::new(move |_, _, update| {
                let done = done.clone();
                Box::pin(async move {
                    update(AgentToolResult::text("progress"));
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    done.fetch_add(1, Ordering::SeqCst);
                    Ok(metadata(3, false))
                })
            }),
        ),
    ];
    rpi_agent::run_agent_loop(
        vec![common::user_message("run")],
        AgentContext {
            tools,
            ..Default::default()
        },
        common::base_config(),
        Arc::new(PanickingProgressEmitter),
        common::mock_stream_fn(vec![
            common::assistant_tool_calls(
                vec![("parent", "codemode", json!({"value":"ok"}))],
                StopReason::ToolUse,
            ),
            common::assistant_text("done", StopReason::Stop),
        ]),
    )
    .await
    .unwrap();
    assert_eq!(cleaned.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retained_native_update_does_not_keep_tool_programs_alive() {
    use std::sync::Weak;
    let saved: Arc<Mutex<Option<Update>>> = Arc::new(Mutex::new(None));
    let programs: Arc<Mutex<Vec<Weak<ToolProgram>>>> = Arc::new(Mutex::new(Vec::new()));
    let root_programs = programs.clone();
    let child_programs = programs.clone();
    let retained = saved.clone();
    let (events, _) = parity_run(
        vec![
            parity_tool(
                "codemode",
                Arc::new(move |p, _, _| {
                    root_programs.lock().unwrap().push(Arc::downgrade(&p));
                    Box::pin(async move {
                        let id = start(&p).await;
                        assert_eq!(poll(&p, &id).await["status"], "done");
                        Ok(AgentToolResult::text("done"))
                    })
                }),
            ),
            parity_tool(
                "effect",
                Arc::new(move |p, _, update| {
                    // ToolProgram -> context.tools -> this closure -> saved update.
                    // The saved native callback must not close that ownership cycle.
                    child_programs.lock().unwrap().push(Arc::downgrade(&p));
                    *retained.lock().unwrap() = Some(update.clone());
                    Box::pin(async move {
                        update(AgentToolResult::text("private progress"));
                        Ok(AgentToolResult::text("done"))
                    })
                }),
            ),
        ],
        common::base_config(),
    )
    .await;
    assert_eq!(programs.lock().unwrap().len(), 2);
    // A worker can still be returning after releasing its drain permit.
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while programs
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.strong_count() != 0)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("retained callback must not own either program");
    let callback = saved.lock().unwrap().as_ref().unwrap().clone();
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        callback(AgentToolResult::text("late private update"));
    }))
    .is_ok());
    assert!(programs
        .lock()
        .unwrap()
        .iter()
        .all(|p| p.upgrade().is_none()));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, rpi_agent::AgentEvent::ToolExecutionUpdate { .. }))
            .count(),
        3,
        "only started, progress and completed reach the root callback"
    );
}

#[tokio::test]
async fn program_close_and_finish_leave_containing_native_signal_alive() {
    for close in ["finish", "cancel_all", "binding"] {
        let saved = Arc::new(Mutex::new(None));
        let observed = saved.clone();
        let (_, result) = parity_run(
            vec![
                parity_tool(
                    "codemode",
                    Arc::new(move |p, signal, _| {
                        let saved = saved.clone();
                        Box::pin(async move {
                            let id = start(&p).await;
                            assert_eq!(poll(&p, &id).await["status"], "done");
                            match close {
                                "cancel_all" => p.cancel_all(),
                                "binding" => {
                                    let driver = CancellationToken::new();
                                    p.bind_signal(driver.clone());
                                    driver.cancel();
                                    tokio::time::timeout(
                                        std::time::Duration::from_secs(3),
                                        async {
                                            while p.handle(json!({"op":"list"})).await.is_ok() {
                                                tokio::task::yield_now().await;
                                            }
                                        },
                                    )
                                    .await
                                    .unwrap();
                                }
                                _ => {}
                            }
                            assert!(
                                !signal.is_cancelled(),
                                "{close} cancelled native postprocessing"
                            );
                            *saved.lock().unwrap() = Some((p, signal));
                            Ok(AgentToolResult::text("postprocessing succeeded"))
                        })
                    }),
                ),
                parity_tool(
                    "effect",
                    Arc::new(|_, _, _| Box::pin(async { Ok(AgentToolResult::text("child done")) })),
                ),
            ],
            common::base_config(),
        )
        .await;
        assert!(!result.is_error);
        let (program, signal) = observed.lock().unwrap().take().unwrap();
        assert!(
            signal.is_cancelled(),
            "containing native signal must retire after execute and postprocessing finish"
        );
        assert!(program.handle(json!({"op":"list"})).await.is_err());
        assert!(start_result(&program).await.is_err());
    }
}

#[tokio::test]
async fn containing_native_cancellation_reaches_program_and_drains_children() {
    let ready = Arc::new(tokio::sync::Notify::new());
    let started = ready.clone();
    let cleaned = Arc::new(AtomicUsize::new(0));
    let observed = cleaned.clone();
    let (_, result) = parity_run(
        vec![
            parity_tool(
                "codemode",
                Arc::new(move |p, signal, _| {
                    let ready = ready.clone();
                    Box::pin(async move {
                        let id = start(&p).await;
                        tokio::time::timeout(std::time::Duration::from_secs(3), ready.notified())
                            .await
                            .unwrap();
                        signal.cancel();
                        assert!(p.handle(json!({"op":"list"})).await.is_err());
                        assert!(start_result(&p).await.is_err());
                        assert_eq!(poll(&p, &id).await["status"], "failed");
                        Ok(AgentToolResult::text("cancelled and drained"))
                    })
                }),
            ),
            parity_tool(
                "effect",
                Arc::new(move |_, signal, _| {
                    let ready = started.clone();
                    let cleaned = cleaned.clone();
                    Box::pin(async move {
                        ready.notify_one();
                        tokio::time::timeout(std::time::Duration::from_secs(3), signal.cancelled())
                            .await
                            .unwrap();
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        cleaned.fetch_add(1, Ordering::SeqCst);
                        Ok(metadata(3, false))
                    })
                }),
            ),
        ],
        common::base_config(),
    )
    .await;
    assert_eq!(observed.load(Ordering::SeqCst), 1);
    assert_eq!(result.usage.unwrap().total_tokens, 3);
}

// Neutral AgentTool JSON is business data, not extension ABI transport metadata.
// In particular, these nested names do not claim the top-level __rpi binding.
fn business_json() -> Value {
    json!({"record": {
        "runtimeKey":"customer-key", "runtimeHandle":{"id":7},
        "__rpi":{"owner":"customer"}, "__rpiRuntimeKey":"customer-runtime",
        "items":[{"runtimeKey":"nested-key", "runtimeHandle":"nested-handle",
            "__rpi":{"runtimeKey":"deep-key"}, "__rpiRuntimeKey":"nested-runtime"}]
    }})
}
fn business_result(args: Value) -> AgentToolResult {
    AgentToolResult {
        content: vec![rpi_agent::TextContentOrImage::Text(
            serde_json::from_value(json!({"type":"text", "text":"business result",
                "textSignature":"business-signature"}))
            .unwrap(),
        )],
        details: args,
        ..Default::default()
    }
}
// Compare the finalized native event, not just a hand-written expected envelope.
fn finalized_json(result: &AgentToolResult) -> Value {
    let content: Vec<_> = result
        .content
        .iter()
        .map(|c| match c {
            rpi_agent::TextContentOrImage::Text(t) => serde_json::to_value(t).unwrap(),
            rpi_agent::TextContentOrImage::Image(i) => serde_json::to_value(i).unwrap(),
        })
        .collect();
    json!({"content":content, "details":result.details, "usage":result.usage,
        "addedToolNames":result.added_tool_names, "terminate":result.terminate})
}
struct BusinessEcho {
    schema: Tool,
    effects: Arc<Mutex<Vec<Value>>>,
}
#[async_trait::async_trait]
impl AgentTool for BusinessEcho {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "business"
    }
    fn prepare_arguments(&self, args: Value) -> Result<Value, AgentError> {
        self.effects.lock().unwrap().push(json!({"prepare":args}));
        Ok(args)
    }
    async fn execute(
        &self,
        _: &str,
        args: Value,
        _: CancellationToken,
        _: Update,
    ) -> Result<AgentToolResult, AgentError> {
        self.effects.lock().unwrap().push(json!({"execute":args}));
        Ok(business_result(args))
    }
}
async fn differential_business_run(nested: bool, hooks: bool) -> (Value, Vec<Value>) {
    let effects = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::new(Mutex::new(None));
    let reply = observed.clone();
    let mut cfg = common::base_config();
    if hooks {
        let seen = effects.clone();
        cfg.before_tool_call = Some(Arc::new(move |ctx, _| {
            let result = if ctx.tool_call.name == "business" {
                seen.lock().unwrap().push(json!({"before":ctx.args}));
                Some(BeforeToolCallResult {
                    args: Some(json!({"hookReplacement":business_json()})),
                    ..Default::default()
                })
            } else {
                None
            };
            Box::pin(async move { result })
        }));
        let seen = effects.clone();
        cfg.after_tool_call = Some(Arc::new(move |ctx, _| {
            let result = if ctx.tool_call.name == "business" {
                seen.lock().unwrap().push(json!({"afterArgs":ctx.args,
                    "afterResult":finalized_json(ctx.result), "isError":ctx.is_error}));
                let finalized = business_result(json!({"hookFinalized":business_json()}));
                Some(AfterToolCallResult {
                    content: Some(finalized.content),
                    details: Some(finalized.details),
                    ..Default::default()
                })
            } else {
                None
            };
            Box::pin(async move { result })
        }));
    }
    let tools: Vec<Arc<dyn AgentTool>> = vec![
        Arc::new(BusinessEcho {
            schema: serde_json::from_value(json!({"name":"business", "description":"echo JSON",
                "parameters":{"type":"object", "properties":{"record":{"type":"object"}},
                    "required":["record"], "additionalProperties":false}}))
            .unwrap(),
            effects: effects.clone(),
        }),
        parity_tool(
            "codemode",
            Arc::new(move |p, _, _| {
                let reply = reply.clone();
                Box::pin(async move {
                    let started = p
                        .handle(json!({"op":"start", "name":"business",
                    "arguments":business_json()}))
                        .await
                        .unwrap();
                    *reply.lock().unwrap() = Some(poll(&p, &started["requestId"]).await);
                    Ok(AgentToolResult::text("parent"))
                })
            }),
        ),
    ];
    let name = if nested { "codemode" } else { "business" };
    let (events, messages) = common::run_and_collect(
        vec![common::user_message("run")],
        AgentContext {
            tools,
            ..Default::default()
        },
        cfg,
        common::mock_stream_fn(vec![
            common::assistant_tool_calls(
                vec![(
                    "parent",
                    name,
                    if nested {
                        json!({"value":"body"})
                    } else {
                        business_json()
                    },
                )],
                StopReason::ToolUse,
            ),
            common::assistant_text("done", StopReason::Stop),
        ]),
    )
    .await;
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m, rpi_agent::AgentMessage::ToolResult(_)))
            .count(),
        1
    );
    let finalized: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            rpi_agent::AgentEvent::ToolExecutionEnd {
                result, is_error, ..
            } => {
                assert!(!is_error, "outer execution failed: {result:?}");
                Some(finalized_json(result))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        finalized.len(),
        1,
        "nested children must not emit ordinary events"
    );
    let result = if nested {
        let child = observed.lock().unwrap().take().unwrap();
        assert_eq!(child["status"], "done", "{child}");
        assert_eq!(child["isError"], false, "{child}");
        child["result"].clone()
    } else {
        finalized[0].clone()
    };
    let effects = effects.lock().unwrap().clone();
    (result, effects)
}
#[tokio::test]
async fn differential_business_json_and_signed_result_survive_nested_execution() {
    let direct = differential_business_run(false, false).await;
    assert_eq!(direct.0, finalized_json(&business_result(business_json())));
    assert_eq!(
        direct.1,
        vec![
            json!({"prepare":business_json()}),
            json!({"execute":business_json()})
        ]
    );
    let nested = differential_business_run(true, false).await;
    assert_eq!(
        nested.1, direct.1,
        "native parameter preparation/execution must match"
    );
    assert_eq!(
        nested.0, direct.0,
        "finalized details and textSignature must match"
    );
}
#[tokio::test]
async fn differential_hooks_preserve_business_json_rewrites_and_finalized_results() {
    let direct = differential_business_run(false, true).await;
    let replacement = json!({"hookReplacement":business_json()});
    assert_eq!(
        direct.1,
        vec![
            json!({"prepare":business_json()}),
            json!({"before":business_json()}),
            json!({"execute":replacement}),
            json!({"afterArgs":replacement, "afterResult":finalized_json(&business_result(replacement.clone())), "isError":false}),
        ]
    );
    assert_eq!(
        direct.0,
        finalized_json(&business_result(json!({"hookFinalized":business_json()})))
    );
    let nested = differential_business_run(true, true).await;
    assert_eq!(
        nested.1, direct.1,
        "native hooks and side effects must match exactly"
    );
    assert_eq!(
        nested.0, direct.0,
        "after-hook finalized business result must match"
    );
}

#[tokio::test]
async fn differential_catalog_uses_effective_batch_snapshot_not_added_names() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let nested = observed.clone();
    let program = parity_tool(
        "codemode",
        Arc::new(move |p, _, _| {
            let nested = nested.clone();
            Box::pin(async move {
                let page = p.handle(json!({"op":"list"})).await.unwrap();
                assert_eq!(page["tools"].as_array().unwrap().len(), 1);
                assert_eq!(page["tools"][0]["name"], "effect");
                assert!(p
                    .handle(
                        json!({"op":"start", "name":"announced-only", "arguments":{"value":"ok"}})
                    )
                    .await
                    .is_err());
                let id = start(&p).await;
                let result = poll(&p, &id).await;
                assert_eq!(result["status"], "done", "{result}");
                assert_eq!(result["isError"], false, "{result}");
                nested
                    .lock()
                    .unwrap()
                    .push(json!({"catalog":page["tools"], "result":result["result"]}));
                Ok(AgentToolResult::text("parent"))
            })
        }),
    );
    let effects = Arc::new(AtomicUsize::new(0));
    let make_effect = |version: &'static str| {
        let effects = effects.clone();
        let mut tool_schema = schema("effect");
        tool_schema.description = version.into();
        Arc::new(ParityTool {
            schema: tool_schema,
            body: Arc::new(move |_, _, _| {
                let effects = effects.clone();
                Box::pin(async move {
                    effects.fetch_add(1, Ordering::SeqCst);
                    Ok(AgentToolResult {
                        added_tool_names: vec!["announced-only".into()],
                        ..AgentToolResult::text(version)
                    })
                })
            }),
        }) as Arc<dyn AgentTool>
    };
    let first = make_effect("batch-one");
    let second = make_effect("batch-two");
    let next_tools = vec![program.clone(), second];
    let turns = Arc::new(AtomicUsize::new(0));
    let mut cfg = common::base_config();
    cfg.prepare_next_turn = Some(Arc::new(move |ctx| {
        let update = if turns.fetch_add(1, Ordering::SeqCst) == 0 {
            let mut context = ctx.context.clone();
            context.tools = next_tools.clone();
            Some(rpi_agent::AgentLoopTurnUpdate {
                context: Some(context),
                ..Default::default()
            })
        } else {
            None
        };
        Box::pin(async move { update })
    }));
    let batch = || {
        common::assistant_tool_calls(
            vec![
                ("direct", "effect", json!({"value":"ok"})),
                ("parent", "codemode", json!({"value":"body"})),
            ],
            StopReason::ToolUse,
        )
    };
    let (events, _) = common::run_and_collect(
        vec![common::user_message("run")],
        AgentContext {
            tools: vec![program, first],
            ..Default::default()
        },
        cfg,
        common::mock_stream_fn(vec![
            batch(),
            batch(),
            common::assistant_text("done", StopReason::Stop),
        ]),
    )
    .await;
    let direct: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            rpi_agent::AgentEvent::ToolExecutionEnd {
                tool_name,
                result,
                is_error,
                ..
            } => {
                assert!(!is_error, "{result:?}");
                (tool_name == "effect").then(|| finalized_json(result))
            }
            _ => None,
        })
        .collect();
    assert_eq!(direct.len(), 2);
    let nested = observed.lock().unwrap();
    assert_eq!(nested.len(), 2);
    for (i, version) in ["batch-one", "batch-two"].iter().enumerate() {
        assert_eq!(direct[i]["content"][0]["text"], *version);
        assert_eq!(nested[i]["catalog"][0]["description"], *version);
        assert_eq!(
            nested[i]["result"], direct[i],
            "direct and child must execute the same batch authority"
        );
    }
    assert_eq!(
        effects.load(Ordering::SeqCst),
        4,
        "one direct and one child execution per batch"
    );
}

async fn reference_start(p: &Arc<ToolProgram>, name: &str) -> Value {
    p.handle(json!({"op":"start","name":name,"arguments":{"value":"ok"},"resultMode":"reference"}))
        .await
        .unwrap()["requestId"]
        .clone()
}

#[tokio::test]
async fn reference_large_finalized_unicode_receipt_reads_losslessly_and_dispose_is_not_release() {
    let saved = Arc::new(Mutex::new(None));
    let retained = saved.clone();
    let mut native = business_result(business_json());
    native.content.push(rpi_agent::TextContentOrImage::text(
        "界😀\"\\n".repeat(150_000),
    ));
    native.details["large"] = json!("é😀\"\\n".repeat(150_000));
    let encoded = serde_json::to_string(&native).unwrap();
    assert!(encoded.len() > 1024 * 1024);
    let actual = native.clone();
    parity_run(vec![
        parity_tool("codemode", Arc::new(move |p, _, _| {
            let encoded = encoded.clone();
            let retained = retained.clone();
            Box::pin(async move {
                for invalid in [json!(null), json!(false), json!(1), json!("inline"), json!({})] {
                    assert!(p.handle(json!({"op":"start","name":"effect","arguments":{"value":"ok"},"resultMode":invalid})).await.is_err());
                }
                let id = reference_start(&p, "effect").await;
                let reply = poll(&p, &id).await;
                assert_eq!(reply, json!({"requestId":id,"status":"done","isError":false,
                    "resultReference":{"version":1,"id":id,"byteLength":encoded.len(),"contentBlocks":2,"isError":false,"terminate":false}}));
                assert!(reply.get("result").is_none());
                let mut offset = 0;
                let mut rebuilt = String::new();
                loop {
                    let chunk = p.handle(json!({"op":"read_result","resultId":id,"offset":offset,"limit":65536})).await.unwrap();
                    assert!(chunk.to_string().len() <= 1024 * 1024);
                    assert_eq!(chunk["offset"], offset);
                    assert_eq!(chunk["byteLength"], encoded.len());
                    rebuilt.push_str(chunk["data"].as_str().unwrap());
                    match chunk["nextOffset"].as_u64() { Some(next) => { assert!(next > offset); offset = next; }, None => break }
                }
                assert_eq!(rebuilt, encoded);
                let mid = encoded.find('界').unwrap() + 1;
                for offset in [json!(mid), json!(encoded.len()+1), json!(-1), json!(1.5), json!("0")] {
                    assert!(p.handle(json!({"op":"read_result","resultId":id,"offset":offset,"limit":4})).await.is_err());
                }
                for limit in [json!(0), json!(3), json!(65537), json!(4.0), json!(-1)] {
                    assert!(p.handle(json!({"op":"read_result","resultId":id,"offset":0,"limit":limit})).await.is_err());
                }
                let eof = p.handle(json!({"op":"read_result","resultId":id,"offset":encoded.len(),"limit":4})).await.unwrap();
                assert_eq!(eof["data"], ""); assert!(eof["nextOffset"].is_null());
                let at = encoded.find('😀').unwrap();
                let floor = p.handle(json!({"op":"read_result","resultId":id,"offset":at-3,"limit":4})).await.unwrap();
                assert_eq!(floor["nextOffset"], at); // Cannot cut a four-byte code point.
                p.handle(json!({"op":"dispose","requestId":id})).await.unwrap();
                assert!(p.handle(json!({"op":"poll","requestId":id})).await.is_err());
                let (attached, error) = p.resolve_result_reference(id.as_str().unwrap()).unwrap();
                assert!(!error); assert_eq!(finalized_json(&attached), serde_json::from_str::<Value>(&encoded).unwrap());
                assert!(p.handle(json!({"op":"read_result","resultId":id,"limit":4})).await.is_ok());
                assert!(p.resolve_result_reference("tp-foreign").is_err());
                assert!(p.handle(json!({"op":"release_result","resultId":"tp-foreign"})).await.is_err());
                p.handle(json!({"op":"release_result","resultId":id})).await.unwrap();
                assert!(p.resolve_result_reference(id.as_str().unwrap()).is_err());
                assert!(p.handle(json!({"op":"read_result","resultId":id})).await.is_err());
                assert!(p.handle(json!({"op":"release_result","resultId":id})).await.is_err());
                let id = reference_start(&p, "effect").await;
                assert_eq!(poll(&p, &id).await["status"], "done");
                *retained.lock().unwrap() = Some((p, id));
                Ok(AgentToolResult::text("parent"))
            })
        })),
        parity_tool("effect", Arc::new(move |_, _, _| { let actual = actual.clone(); Box::pin(async move { Ok(actual) }) })),
    ], common::base_config()).await;
    let (p, id) = saved.lock().unwrap().take().unwrap();
    assert!(p.resolve_result_reference(id.as_str().unwrap()).is_err());
    assert!(p
        .handle(json!({"op":"read_result","resultId":id}))
        .await
        .is_err());
    // Finish clears storage even if an external Arc keeps the program alive.
    assert!(p
        .handle(json!({"op":"release_result","resultId":id}))
        .await
        .is_err());
}

#[tokio::test]
async fn reference_native_attachment_authority_metadata_once_and_termination() {
    for error in [false, true] {
        for empty in [false, true] {
            let (_, message) = parity_run(
                vec![
                    parity_tool(
                        "codemode",
                        Arc::new(move |p, _, _| {
                            Box::pin(async move {
                                let id = reference_start(&p, "effect").await;
                                let reply = poll(&p, &id).await;
                                assert_eq!(reply["status"], "done");
                                assert_eq!(reply["isError"], error);
                                assert!(reply.get("result").is_none());
                                assert_eq!(reply["resultReference"]["terminate"], true);
                                let inspection = p
                                    .handle(json!({"op":"read_result","resultId":id}))
                                    .await
                                    .unwrap();
                                let inspection: Value =
                                    serde_json::from_str(inspection["data"].as_str().unwrap())
                                        .unwrap();
                                assert_eq!(inspection["usage"]["totalTokens"], 3);
                                assert_eq!(inspection["addedToolNames"], json!(["z", "a", "z"]));
                                assert!(start_result(&p).await.is_err());
                                for _ in 0..3 {
                                    let (native, native_error) =
                                        p.resolve_result_reference(id.as_str().unwrap()).unwrap();
                                    assert_eq!(native_error, error);
                                    assert!(native.usage.is_none());
                                    assert!(native.added_tool_names.is_empty());
                                    assert!(native.terminate);
                                }
                                let (native, native_error) =
                                    p.resolve_result_reference(id.as_str().unwrap()).unwrap();
                                if native_error {
                                    Err(AgentError::StructuredTool(Box::new(native)))
                                } else {
                                    Ok(native)
                                }
                            })
                        }),
                    ),
                    parity_tool(
                        "effect",
                        Arc::new(move |_, _, _| {
                            Box::pin(async move {
                                let mut native = metadata(3, true);
                                native.content = if empty {
                                    vec![]
                                } else {
                                    business_result(business_json()).content
                                };
                                native.details = business_json();
                                if error {
                                    Err(AgentError::StructuredTool(Box::new(native)))
                                } else {
                                    Ok(native)
                                }
                            })
                        }),
                    ),
                ],
                common::base_config(),
            )
            .await;
            assert_eq!(message.is_error, error);
            assert_eq!(message.usage.unwrap().total_tokens, 3);
            assert_eq!(message.added_tool_names, vec!["a", "z"]);
            assert_eq!(message.content.len(), if empty { 0 } else { 1 });
            if !empty {
                assert_eq!(
                    serde_json::to_value(&message.content[0]).unwrap()["textSignature"],
                    "business-signature"
                );
            }
        }
    }
}

#[tokio::test]
async fn reference_count_limit_release_and_cancel_cleanup_keep_accounting() {
    let (_, result) = parity_run(
        vec![
            parity_tool(
                "codemode",
                Arc::new(|p, _, _| {
                    Box::pin(async move {
                        let mut ids = vec![];
                        for i in 0..17 {
                            let id = reference_start(&p, "effect").await;
                            let reply = poll(&p, &id).await;

                            if i == 16 {
                                assert_eq!(reply["status"], "failed");
                                assert!(reply["error"]
                                    .as_str()
                                    .unwrap()
                                    .contains("reference limit"));
                            } else {
                                assert_eq!(reply["status"], "done");
                                ids.push(id.clone());
                            }
                            p.handle(json!({"op":"dispose","requestId":id}))
                                .await
                                .unwrap();
                        }
                        p.handle(json!({"op":"release_result","resultId":ids[0]}))
                            .await
                            .unwrap();
                        let id = reference_start(&p, "effect").await;
                        assert_eq!(poll(&p, &id).await["status"], "done");
                        p.cancel_all();
                        assert!(p.resolve_result_reference(id.as_str().unwrap()).is_err());
                        assert!(p
                            .handle(json!({"op":"read_result","resultId":id}))
                            .await
                            .is_err());
                        p.handle(json!({"op":"release_result","resultId":id}))
                            .await
                            .unwrap();
                        Ok(AgentToolResult::text("parent"))
                    })
                }),
            ),
            parity_tool(
                "effect",
                Arc::new(|_, _, _| Box::pin(async { Ok(metadata(3, false)) })),
            ),
        ],
        common::base_config(),
    )
    .await;
    assert_eq!(result.usage.unwrap().total_tokens, 18 * 3);
}

#[tokio::test]
async fn reference_shared_tree_quota_and_foreign_programs_are_enforced() {
    let outer = Arc::new(Mutex::new(None));
    let saved = outer.clone();
    let (_, result) = parity_run(
        vec![
            parity_tool(
                "codemode",
                Arc::new(move |p, _, _| {
                    let saved = saved.clone();
                    Box::pin(async move {
                        let id = reference_start(&p, "effect").await;
                        assert_eq!(poll(&p, &id).await["status"], "done");
                        *saved.lock().unwrap() = Some((p.clone(), id));
                        let id = p
                            .handle(
                                json!({"op":"start","name":"nested","arguments":{"value":"ok"}}),
                            )
                            .await
                            .unwrap()["requestId"]
                            .clone();
                        assert_eq!(poll(&p, &id).await["status"], "done");
                        // Nested finish returned its permits despite retaining a receipt.
                        let id = reference_start(&p, "effect").await;
                        assert_eq!(poll(&p, &id).await["status"], "done");
                        Ok(AgentToolResult::text("parent"))
                    })
                }),
            ),
            parity_tool(
                "nested",
                Arc::new(move |p, _, _| {
                    let outer = outer.clone();
                    Box::pin(async move {
                        let (parent, foreign) = outer.lock().unwrap().take().unwrap();
                        assert!(p
                            .resolve_result_reference(foreign.as_str().unwrap())
                            .is_err());
                        assert!(p
                            .handle(json!({"op":"measure_result_content","resultId":foreign}))
                            .await
                            .is_err());
                        assert!(p
                            .resolve_content_parts(vec![content_part(&foreign, None)])
                            .is_err());
                        assert!(p
                            .handle(json!({"op":"read_result","resultId":foreign}))
                            .await
                            .is_err());
                        assert!(p
                            .handle(json!({"op":"release_result","resultId":foreign}))
                            .await
                            .is_err());
                        let id = reference_start(&p, "effect").await;
                        let failed = poll(&p, &id).await;
                        assert_eq!(failed["status"], "failed");
                        assert!(failed["error"].as_str().unwrap().contains("byte quota"));
                        parent
                            .handle(json!({"op":"release_result","resultId":foreign}))
                            .await
                            .unwrap();
                        let id = reference_start(&p, "effect").await;
                        assert_eq!(poll(&p, &id).await["status"], "done");
                        assert!(parent
                            .resolve_result_reference(id.as_str().unwrap())
                            .is_err());
                        Ok(AgentToolResult::text("nested"))
                    })
                }),
            ),
            parity_tool(
                "effect",
                Arc::new(|_, _, _| {
                    Box::pin(async {
                        let mut r = metadata(3, false);
                        r.details = json!({"big":"x".repeat(18*1024*1024)});
                        Ok(r)
                    })
                }),
            ),
        ],
        common::base_config(),
    )
    .await;
    assert_eq!(result.usage.unwrap().total_tokens, 12);
}

#[tokio::test]
async fn reference_per_result_overflow_still_retains_termination_and_finalized_accounting() {
    let (_, message) = parity_run(
        vec![
            parity_tool(
                "codemode",
                Arc::new(|p, _, _| {
                    Box::pin(async move {
                        let id = reference_start(&p, "effect").await;
                        let reply = poll(&p, &id).await;
                        assert_eq!(reply["status"], "failed");
                        assert!(reply["error"].as_str().unwrap().contains("byte quota"));
                        assert!(p.resolve_result_reference(id.as_str().unwrap()).is_err());
                        assert!(start_result(&p).await.is_err());
                        Ok(AgentToolResult::text("parent"))
                    })
                }),
            ),
            parity_tool(
                "effect",
                Arc::new(|_, _, _| {
                    Box::pin(async {
                        let mut r = metadata(3, true);
                        r.details = json!({"big":"x".repeat(32*1024*1024)});
                        Ok(r)
                    })
                }),
            ),
        ],
        common::base_config(),
    )
    .await;
    assert_eq!(message.usage.unwrap().total_tokens, 3);
}

#[tokio::test]
async fn reference_finish_clears_receipts_after_error_panic_and_timeout_drain() {
    use futures::FutureExt;
    for mode in ["error", "panic", "timeout"] {
        let saved = Arc::new(Mutex::new(None));
        let observed = saved.clone();
        let cleaned = Arc::new(AtomicUsize::new(0));
        let done = cleaned.clone();
        let execution = parity_run(
            vec![
                parity_tool(
                    "codemode",
                    Arc::new(move |p, _, _| {
                        let saved = saved.clone();
                        Box::pin(async move {
                            let id = p.handle(json!({"op":"start","name":"effect","arguments":{"value":"ok"},"resultMode":"reference","timeoutMs":if mode == "timeout" {1} else {1000}})).await.unwrap()["requestId"].clone();
                            assert_eq!(
                                poll(&p, &id).await["status"],
                                if mode == "timeout" { "failed" } else { "done" }
                            );
                            *saved.lock().unwrap() = Some((p, id));
                            if mode == "panic" {
                                panic!("reference owner panic");
                            }
                            Err(AgentError::tool("reference owner error"))
                        })
                    }),
                ),
                parity_tool(
                    "effect",
                    Arc::new(move |_, _, _| {
                        let done = done.clone();
                        Box::pin(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                            done.fetch_add(1, Ordering::SeqCst);
                            Ok(metadata(3, false))
                        })
                    }),
                ),
            ],
            common::base_config(),
        );
        let outcome = std::panic::AssertUnwindSafe(execution).catch_unwind().await;
        assert_eq!(outcome.is_err(), mode == "panic");
        if let Ok((_, message)) = outcome {
            assert_eq!(message.usage.unwrap().total_tokens, 3);
        }
        assert_eq!(
            cleaned.load(Ordering::SeqCst),
            1,
            "finish drained native work"
        );
        let (p, id) = observed.lock().unwrap().take().unwrap();
        assert!(p.resolve_result_reference(id.as_str().unwrap()).is_err());
        assert!(
            p.handle(json!({"op":"release_result","resultId":id}))
                .await
                .is_err(),
            "finish cleared the receipt, even after late timeout finalization"
        );
    }
}

fn content_part(
    id: &Value,
    indices: Option<Vec<usize>>,
) -> rpi_agent::tool_program::ToolProgramContentPart {
    rpi_agent::tool_program::ToolProgramContentPart::Reference {
        id: id.as_str().unwrap().to_owned(),
        indices,
    }
}
fn content_wire(content: Vec<rpi_agent::TextContentOrImage>) -> Value {
    finalized_json(&AgentToolResult {
        content,
        ..Default::default()
    })["content"]
        .clone()
}
#[tokio::test]
async fn content_measure_selection_wire_bytes_and_native_lifetime() {
    use rpi_agent::{tool_program::ToolProgramContentPart as Part, TextContentOrImage as Block};
    let native = AgentToolResult {
        content: vec![
            Block::Text(
                serde_json::from_value(json!({"type":"text","text":"界😀
\"","textSignature":"sig-é"}))
                .unwrap(),
            ),
            Block::Image(
                serde_json::from_value(
                    json!({"type":"image","mimeType":"image/png","data":"YWJj"}),
                )
                .unwrap(),
            ),
            Block::text("tail"),
        ],
        details: json!({"not":"selected"}),
        ..Default::default()
    };
    let wire = content_wire(native.content.clone());
    let saved = Arc::new(Mutex::new(None));
    let retained = saved.clone();
    parity_run(vec![
        parity_tool("codemode", Arc::new(move |p, _, _| {
            let wire = wire.clone(); let retained = retained.clone();
            Box::pin(async move {
                let id = reference_start(&p, "effect").await;
                assert_eq!(poll(&p, &id).await["status"], "done");
                for indices in [None, Some(json!(null)), Some(json!([])), Some(json!([2,0,2,1])), Some(json!([0]))] {
                    let mut args = json!({"op":"measure_result_content","resultId":id});
                    if let Some(indices) = &indices { args["indices"] = indices.clone(); }
                    let selected: Vec<usize> = match &indices {
                        Some(Value::Array(a)) => a.iter().map(|i| i.as_u64().unwrap() as usize).collect(),
                        _ => vec![0,1,2],
                    };
                    let bytes: usize = selected.iter().map(|i| serde_json::to_vec(&wire[*i]).unwrap().len()).sum();
                    let measured = p.handle(args).await.unwrap();
                    assert_eq!(measured, json!({"resultId":id,"contentBlocks":selected.len(),"encodedBytes":bytes}));
                    assert!(measured.get("data").is_none());
                    let blocks = p.resolve_content_parts(vec![content_part(&id, Some(selected.clone()))]).unwrap();
                    assert_eq!(content_wire(blocks), Value::Array(selected.iter().map(|i| wire[*i].clone()).collect()));
                }
                for indices in [json!([-1]),json!([1.0]),json!([3]),json!(false),json!(["0"]),json!(vec![0;257])] {
                    assert!(p.handle(json!({"op":"measure_result_content","resultId":id,"indices":indices})).await.is_err());
                }
                assert!(p.resolve_content_parts(vec![content_part(&id, Some(vec![3]))]).is_err());
                let mixed = p.resolve_content_parts(vec![Part::Inline(vec![Block::text("prefix")]),content_part(&id,Some(vec![0,1])),Part::Inline(vec![Block::text("suffix")])]).unwrap();
                assert_eq!(mixed.len(),4);
                assert_eq!(content_wire(mixed)[1],wire[0]);
                assert!(p.handle(json!({"op":"measure_result_content","resultId":"foreign"})).await.is_err());
                assert!(p.resolve_content_parts(vec![content_part(&json!("foreign"),None)]).is_err());
                p.handle(json!({"op":"dispose","requestId":id})).await.unwrap();
                assert!(p.handle(json!({"op":"measure_result_content","resultId":id})).await.is_ok());
                p.handle(json!({"op":"release_result","resultId":id})).await.unwrap();
                assert!(p.handle(json!({"op":"measure_result_content","resultId":id,"indices":[]})).await.is_err());
                assert!(p.resolve_content_parts(vec![content_part(&id,Some(vec![]))]).is_err());
                let id = reference_start(&p,"effect").await;
                assert_eq!(poll(&p,&id).await["status"],"done");
                p.cancel_all();
                assert!(p.handle(json!({"op":"measure_result_content","resultId":id})).await.is_err());
                assert!(p.resolve_content_parts(vec![Part::Inline(vec![])]).is_err());
                *retained.lock().unwrap() = Some((p,id));
                Ok(AgentToolResult::text("parent"))
            })
        })),
        parity_tool("effect",Arc::new(move |_,_,_| {let native=native.clone(); Box::pin(async move {Ok(native)})})),
    ],common::base_config()).await;
    let (p, id) = saved.lock().unwrap().take().unwrap();
    assert!(p
        .handle(json!({"op":"measure_result_content","resultId":id}))
        .await
        .is_err());
    assert!(p
        .resolve_content_parts(vec![content_part(&id, None)])
        .is_err());
    assert!(p
        .handle(json!({"op":"release_result","resultId":id}))
        .await
        .is_err());
}

#[tokio::test]
async fn content_parts_count_and_duplicate_expansion_budget() {
    use rpi_agent::{tool_program::ToolProgramContentPart as Part, TextContentOrImage as Block};
    parity_run(vec![
        parity_tool("codemode",Arc::new(|p,_,_| Box::pin(async move {
            let id=reference_start(&p,"effect").await;
            assert_eq!(poll(&p,&id).await["status"],"done");
            assert!(p.resolve_content_parts(vec![Part::Inline(vec![]);257]).is_err());
            assert!(p.resolve_content_parts(vec![Part::Inline(vec![]);256]).unwrap().is_empty());
            let many=reference_start(&p,"many").await;
            assert_eq!(poll(&p,&many).await["status"],"done");
            assert!(p.handle(json!({"op":"measure_result_content","resultId":many})).await.unwrap_err().contains("256"));
            assert!(p.resolve_content_parts(vec![content_part(&many,None)]).is_err());
            assert_eq!(p.handle(json!({"op":"measure_result_content","resultId":many,"indices":[256]})).await.unwrap()["contentBlocks"],1);
            assert_eq!(p.resolve_content_parts(vec![content_part(&many,Some(vec![256]))]).unwrap().len(),1);
            assert!(p.resolve_content_parts(vec![Part::Inline(vec![Block::text("");257])]).is_err());
            assert!(p.resolve_content_parts(vec![content_part(&id,Some(vec![0;257]))]).is_err());
            for copies in [4,128,256] {
                assert!(p.handle(json!({"op":"measure_result_content","resultId":id,"indices":vec![0;copies]})).await.unwrap_err().contains("32 MiB"));
                assert!(p.resolve_content_parts(vec![content_part(&id,Some(vec![0;copies]))]).unwrap_err().contains("32 MiB"));
            }
            assert_eq!(p.resolve_content_parts(vec![content_part(&id,Some(vec![0;3]))]).unwrap().len(),3);
            // Output expansion must neither consume retained-tree quota nor release the source.
            assert!(p.handle(json!({"op":"measure_result_content","resultId":id})).await.is_ok());
            let empty_size=content_wire(vec![Block::text("")]).to_string().len();
            let exact=Block::text("x".repeat(32*1024*1024-empty_size));
            assert_eq!(p.resolve_content_parts(vec![Part::Inline(vec![exact.clone()])]).unwrap().len(),1);
            assert!(p.resolve_content_parts(vec![Part::Inline(vec![exact,Block::text("")])]).unwrap_err().contains("32 MiB"));
            assert_eq!(p.resolve_content_parts(vec![Part::Inline(vec![Block::text("");256])]).unwrap().len(),256);
            Ok(AgentToolResult::text("parent"))
        }))),
        parity_tool("effect",Arc::new(|_,_,_| Box::pin(async {Ok(AgentToolResult::text("x".repeat(8*1024*1024)))}))),
        parity_tool("many",Arc::new(|_,_,_| Box::pin(async {Ok(AgentToolResult {content:vec![Block::text("");257],..Default::default()})}))),
    ],common::base_config()).await;
}
