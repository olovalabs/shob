//! Parallel execute futures are owned by their loop, not detached tasks.
#[path = "common/mod.rs"]
mod common;

use common::{assistant_text, assistant_tool_calls, base_config, mock_stream_fn, user_message};
use futures::future::BoxFuture;
use rpi_agent::{AgentContext, AgentEmitter, AgentEvent, AgentToolResult, CollectorEmitter};
use rpi_ai::types::StopReason;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

type Update = Arc<dyn Fn(rpi_agent::ToolResultPartial) + Send + Sync>;
struct State {
    first_entered: Notify,
    second_entered: Notify,
    release: Notify,
    first_alive: AtomicBool,
    pause_first: bool,
    update: Mutex<Option<Update>>,
}
impl State {
    fn new(pause_first: bool) -> Arc<Self> {
        Arc::new(Self {
            first_entered: Notify::new(),
            second_entered: Notify::new(),
            release: Notify::new(),
            first_alive: AtomicBool::new(false),
            pause_first,
            update: Mutex::new(None),
        })
    }
}
struct Live<'a>(&'a AtomicBool);
impl Drop for Live<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
struct Tool {
    schema: rpi_ai::types::Tool,
    state: Arc<State>,
}
#[async_trait::async_trait]
impl rpi_agent::AgentTool for Tool {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "owned"
    }
    async fn execute(
        &self,
        id: &str,
        _: serde_json::Value,
        _: CancellationToken,
        update: Update,
    ) -> Result<AgentToolResult, rpi_agent::AgentError> {
        if id == "panic" {
            panic!("scripted tool panic");
        }
        if id == "first" {
            self.state.first_alive.store(true, Ordering::SeqCst);
            let _live = Live(&self.state.first_alive);
            *self.state.update.lock().unwrap() = Some(update);
            self.state.first_entered.notify_one();
            if self.state.pause_first {
                self.state.release.notified().await;
            }
        } else {
            self.state.second_entered.notify_one();
        }
        Ok(AgentToolResult::text(id))
    }
}
fn context(state: Arc<State>) -> AgentContext {
    AgentContext {
        system_prompt: String::new(),
        messages: vec![],
        tools: vec![Arc::new(Tool {
            schema: rpi_ai::types::Tool {
                name: "owned".into(),
                description: "owned future test".into(),
                parameters: rpi_ai::types::Schema::new(
                    serde_json::json!({"type":"object","properties":{}}),
                ),
                constrained_sampling: None,
            },
            state,
        })],
    }
}
fn stream(first: &str) -> rpi_agent::StreamFn {
    mock_stream_fn(vec![
        assistant_tool_calls(
            vec![
                (first, "owned", serde_json::json!({})),
                ("second", "owned", serde_json::json!({})),
            ],
            StopReason::ToolUse,
        ),
        assistant_text("done", StopReason::Stop),
    ])
}
async fn wait(notify: &Notify) {
    tokio::time::timeout(std::time::Duration::from_secs(3), notify.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn dropping_loop_drops_live_parallel_execute_and_suppresses_its_updates() {
    let state = State::new(true);
    let (collector, events) = CollectorEmitter::new();
    let caller = tokio::spawn(rpi_agent::run_agent_loop(
        vec![user_message("start")],
        context(state.clone()),
        base_config(),
        Arc::new(collector),
        stream("first"),
    ));
    wait(&state.first_entered).await;
    wait(&state.second_entered).await;
    assert!(state.first_alive.load(Ordering::SeqCst));
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert!(!state.first_alive.load(Ordering::SeqCst));
    let count = events.lock().unwrap().len();
    state.update.lock().unwrap().as_ref().unwrap()(AgentToolResult::text("too late"));
    assert_eq!(events.lock().unwrap().len(), count);
}

#[tokio::test]
async fn pending_preparation_hook_does_not_starve_a_ready_peer() {
    let state = State::new(true);
    let mut config = base_config();
    let coordination = state.clone();
    config.before_tool_call = Some(Arc::new(move |ctx, _| {
        let state = coordination.clone();
        let is_second = ctx.tool_call.id == "second";
        Box::pin(async move {
            if is_second {
                // No executor-spawned task: the owning loop must poll first
                // while this later sequential preparation is still pending.
                state.first_entered.notified().await;
                state.release.notify_one();
            }
            None
        })
    }));
    let (events, _) = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        common::run_and_collect(
            vec![user_message("prepare")],
            context(state),
            config,
            stream("first"),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        common::tool_result_message_ids(&events),
        ["first", "second"]
    );
}

struct Backpressured {
    inner: CollectorEmitter,
    state: Arc<State>,
}
impl AgentEmitter for Backpressured {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        let delivery = self.inner.emit(event.clone());
        let state = self.state.clone();
        Box::pin(async move {
            if matches!(event, AgentEvent::ToolExecutionEnd { ref tool_call_id, .. } if tool_call_id == "first")
            {
                state.second_entered.notified().await;
            }
            delivery.await;
        })
    }
    fn try_emit(&self, event: AgentEvent) {
        self.inner.try_emit(event);
    }
}

#[tokio::test]
async fn backpressured_completion_event_does_not_starve_other_execute_futures() {
    let state = State::new(false);
    let (inner, events) = CollectorEmitter::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        rpi_agent::run_agent_loop(
            vec![user_message("backpressure")],
            context(state.clone()),
            base_config(),
            Arc::new(Backpressured { inner, state }),
            stream("first"),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let events = events.lock().unwrap();
    assert_eq!(common::tool_execution_end_ids(&events), ["first", "second"]);
    assert_eq!(
        common::tool_result_message_ids(&events),
        ["first", "second"]
    );
}

#[tokio::test]
async fn parallel_panic_becomes_an_error_under_the_original_call_identity() {
    let (events, messages) = common::run_and_collect(
        vec![user_message("panic")],
        context(State::new(false)),
        base_config(),
        stream("panic"),
    )
    .await;
    assert_eq!(
        common::tool_result_message_ids(&events),
        ["panic", "second"]
    );
    let results: Vec<_> = messages
        .iter()
        .filter_map(|m| match m {
            rpi_agent::AgentMessage::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results[0].is_error);
    assert_eq!(results[0].tool_name, "owned");
    assert!(!results[1].is_error);
}
