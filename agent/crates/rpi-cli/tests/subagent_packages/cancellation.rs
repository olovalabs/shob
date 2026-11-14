use rpi_agent::{AgentError, AgentTool, AgentToolResult, ToolResultPartial};
use rpi_ai::{
    providers::faux::{FauxScript, FauxStep},
    types::{Schema, Tool},
};
use rpi_harness::{agent_harness::AgentHarness, types::HarnessTool};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

struct WaitForCancellation {
    schema: Tool,
    started: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl AgentTool for WaitForCancellation {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "wait for cancellation"
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        signal: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        self.started.fetch_add(1, Ordering::Release);
        signal.cancelled().await;
        Err(AgentError::Tool("cancelled".into()))
    }
}

pub(super) async fn repeated_cancel_does_not_exhaust_handles(
    parent: &AgentHarness,
    script: &FauxScript,
) {
    let mut tools = parent.get_tools().await.unwrap();
    let delegate = tools
        .iter()
        .find(|t| t.tool.schema().name == "delegate_task")
        .unwrap()
        .tool
        .clone();
    let started = Arc::new(AtomicUsize::new(0));
    tools.push(HarnessTool::new(Arc::new(WaitForCancellation {
        schema: Tool {
            name: "wait_for_cancel".into(),
            description: "test cancellation".into(),
            parameters: Schema::new(json!({"type":"object"})),
            constrained_sampling: None,
        },
        started: started.clone(),
    })));
    parent.set_tools(tools, None).await.unwrap();
    // More than the manager's 16 retained handles. An idle parent harness must
    // not orphan children when just the invoking plugin tool is cancelled.
    for attempt in 0..20 {
        script.set_responses(vec![FauxStep::tool_call("wait_for_cancel", json!({}))]);
        let signal = CancellationToken::new();
        let _cancel_on_failure = signal.clone().drop_guard();
        let tool = delegate.clone();
        let call_signal = signal.clone();
        let mut call = tokio::spawn(async move {
            tool.execute(
                "cancel-repeatedly",
                json!({"task":"wait until cancelled"}),
                call_signal,
                Arc::new(|_| {}),
            )
            .await
        });
        let admitted = tokio::time::timeout(Duration::from_secs(3), async {
            while started.load(Ordering::Acquire) <= attempt {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });
        tokio::select! {
            result = &mut call => panic!("attempt {attempt} failed before admission: {result:?}"),
            result = admitted => result.expect("each child must start without exhausting retained slots"),
        }
        signal.cancel();
        let result = tokio::time::timeout(Duration::from_secs(3), call)
            .await
            .unwrap()
            .unwrap();
        assert!(
            result.is_err(),
            "cancelled invocation must not report success"
        );
    }
}
