use super::*;
use rpi_agent::{AgentError, AgentTool, AgentToolResult, ToolResultPartial};
use rpi_ai::{
    providers::faux::FauxStep,
    types::{Schema, Tool},
};
use rpi_extensions::{RuntimeActionHost, ToolCallContext};
use rpi_harness::types::HarnessTool;
use std::sync::atomic::AtomicBool;

struct Alias {
    schema: Tool,
    host: Arc<dyn RuntimeActionHost>,
}
#[async_trait::async_trait]
impl AgentTool for Alias {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        &self.schema.name
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let context = self.host.get_execution_context(json!({})).await.unwrap();
        assert_eq!(context["tools"], json!(["research_alias"]));
        assert_eq!(context["childAgent"], false);
        let error = self
            .host
            .child_agent(
                7,
                json!({"op":"start","task":"nested","tools":["restricted"]}),
            )
            .await
            .expect_err("child must not reenter the root runtime host");
        assert!(error.contains("recursive"));
        Ok(AgentToolResult::text("nested delegation blocked"))
    }
}
fn schema(name: &str) -> Tool {
    Tool {
        name: name.into(),
        description: "test".into(),
        parameters: Schema::new(json!({"type":"object"})),
        constrained_sampling: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_narrowed_child_cannot_reenter_the_root_host_through_an_alias_tool() {
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::tool_call("research_alias", json!({})),
        FauxStep::text("blocked report"),
    ]);
    let parent = parent(script).await;
    let (host, cell) = crate::extensions_actions::HarnessActionHost::new_empty(
        vec![],
        std::env::temp_dir(),
        tokio::runtime::Handle::current(),
        Default::default(),
        ToolCallContext::default(),
    );
    crate::extensions_actions::HarnessActionHost::set_harness(&cell, Arc::new(parent.clone()));
    let host: Arc<dyn RuntimeActionHost> = Arc::new(host);
    parent
        .set_tools(
            vec![
                HarnessTool::new(Arc::new(Alias {
                    schema: schema("research_alias"),
                    host: host.clone(),
                })),
                HarnessTool::new(Arc::new(Alias {
                    schema: schema("restricted"),
                    host,
                })),
            ],
            None,
        )
        .await
        .unwrap();
    let manager = ChildAgents::default();
    let started = manager
        .handle(
            &parent,
            7,
            json!({"op":"start","task":"outer","tools":["research_alias"]}),
        )
        .await
        .unwrap();
    let done = wait(&manager, &parent, started["childId"].as_str().unwrap()).await;
    assert_eq!(done["status"], "done");
    assert_eq!(done["result"]["text"], "blocked report");
}

struct Slow {
    schema: Tool,
    started: Arc<AtomicBool>,
    panic_after_start: bool,
    observed_signal: Arc<Mutex<Option<CancellationToken>>>,
    release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}
#[async_trait::async_trait]
impl AgentTool for Slow {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "slow"
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        signal: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        *self.observed_signal.lock().unwrap() = Some(signal);
        let guard = rpi_agent::tool_execution_scope::track_background_tool().unwrap();
        let release = self.release.lock().unwrap().take().unwrap();
        std::thread::spawn(move || {
            let _guard = guard;
            let _ = release.recv();
        });
        self.started.store(true, Ordering::SeqCst);
        if self.panic_after_start {
            return Ok(AgentToolResult::text("background work still running"));
        }
        // Deliberately uncooperative foreground, like a blocked foreign driver.
        std::future::pending().await
    }
}
async fn assert_capacity_retained(panic_after_start: bool) {
    let script = FauxScript::new();
    script.set_responses(vec![FauxStep::tool_call("slow", json!({}))]);
    let started = Arc::new(AtomicBool::new(false));
    let flag = started.clone();
    let provider = FauxProvider::new(script);
    let parent = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        transform_context: Some(Arc::new(move |messages, _| {
            let panic_now = panic_after_start && flag.load(Ordering::SeqCst);
            Box::pin(async move {
                assert!(!panic_now, "run panicked after starting background work");
                messages
            })
        })),
        ..Default::default()
    })
    .await
    .unwrap();
    let (release, receiver) = std::sync::mpsc::channel();
    let observed_signal = Arc::new(Mutex::new(None));
    parent
        .set_tools(
            vec![HarnessTool::new(Arc::new(Slow {
                schema: schema("slow"),
                started: started.clone(),
                panic_after_start,
                observed_signal: observed_signal.clone(),
                release: Mutex::new(Some(receiver)),
            }))],
            None,
        )
        .await
        .unwrap();
    let manager = ChildAgents::default();
    let child = manager
        .handle(&parent, 7, json!({"op":"start","task":"slow"}))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    if panic_after_start {
        let done = wait(&manager, &parent, child["childId"].as_str().unwrap()).await;
        assert!(done["error"].as_str().unwrap().contains("panicked"));
    } else {
        manager
            .handle(
                &parent,
                7,
                json!({"op":"dispose","childId":child["childId"]}),
            )
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(if panic_after_start {
        50
    } else {
        2150
    }))
    .await;
    let signal = observed_signal.lock().unwrap().take().unwrap();
    tokio::time::timeout(Duration::from_secs(2), signal.cancelled())
        .await
        .unwrap();
    let retained = manager.active.load(Ordering::Acquire);
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while manager.active.load(Ordering::Acquire) != 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        retained, 1,
        "a detached/blocked driver still consumes cleanup capacity"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_active_capacity_permit_survives_the_two_second_cleanup_grace() {
    assert_capacity_retained(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panicking_run_retains_its_permit_until_background_drivers_exit() {
    assert_capacity_retained(true).await;
}
