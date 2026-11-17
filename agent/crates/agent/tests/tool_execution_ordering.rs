//! THE tool-execution ordering invariant (plan §5.1).
//!
//! Mirrors TS `agent-loop.test.ts`:
//! "should emit tool_execution_end in completion order but persist tool results
//! in source order".
//!
//! Two parallel tool calls (`tool-1` = "first", `tool-2` = "second") in a single
//! assistant message. `tool-1` blocks on a release signal; `tool-2` runs
//! immediately and completes first. Asserts:
//! - `ToolExecutionEnd` ids arrive in **completion** order: `["tool-2","tool-1"]`.
//! - tool-result `MessageEnd` ids + `TurnEnd.toolResults` ids are in **source /
//!   ordinal** order: `["tool-1","tool-2"]`.
//! - genuine parallelism: `tool-2` ran while `tool-1` was still pending.

#[path = "common/mod.rs"]
mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agent::{AgentContext, AgentEvent, ToolExecutionMode};
use ai::event_stream::create_assistant_message_event_stream;
use ai::types::{AssistantMessage, AssistantMessageEvent, DoneReason, StopReason};
use common::{base_config, run_and_collect, user_message};
use tokio::sync::Notify;

/// Shared coordination state for the two-call parallel batch.
struct Coord {
    /// `notify_one()` on this releases the blocking "first" call.
    release: Arc<Notify>,
    /// Set when "first" has completed (mirrors TS `firstResolved`).
    first_resolved: Arc<AtomicBool>,
    /// Set when "second" ran while "first" was still pending (TS
    /// `parallelObserved`).
    parallel_observed: Arc<AtomicBool>,
}

/// The echo tool. For `value == "first"` it awaits `release` before returning;
/// for `value == "second"` it sets `parallel_observed` if `first` hasn't
/// resolved yet.
struct EchoTool {
    schema: ai::types::Tool,
    coord: Coord,
}

#[async_trait::async_trait]
impl agent::AgentTool for EchoTool {
    fn schema(&self) -> &ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "Echo"
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: tokio_util::sync::CancellationToken,
        _on_update: Arc<dyn Fn(agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<agent::AgentToolResult, agent::AgentError> {
        let value = params
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if value == "first" {
            // Block until the producer releases us — mirrors `await firstDone`.
            self.coord.release.notified().await;
            self.coord.first_resolved.store(true, Ordering::SeqCst);
        }
        if value == "second" && !self.coord.first_resolved.load(Ordering::SeqCst) {
            self.coord.parallel_observed.store(true, Ordering::SeqCst);
        }
        Ok(agent::AgentToolResult::text(format!("echoed: {value}")))
    }
}

fn echo_schema() -> ai::types::Tool {
    ai::types::Tool {
        name: "echo".to_string(),
        description: "Echo tool".to_string(),
        parameters: ai::types::Schema::new(serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "string" } },
            "required": ["value"],
            "additionalProperties": false,
        })),
        constrained_sampling: None,
    }
}

/// A stream_fn mirroring the TS mock: call 1 → assistant message with both tool
/// calls (`toolUse`), then 20ms later releases `tool-1`; call 2 → text "done"
/// (`stop`). The script is shared via `Arc<Mutex<VecDeque>>` so a `Fn` closure
/// (not `FnMut`) can shift one message per call.
fn two_call_stream_fn(release: Arc<Notify>) -> agent::StreamFn {
    use std::collections::VecDeque;
    // The script is dequeued FRONT-FIRST: call 1 first (tool calls), then
    // call 2 (text "done").
    let script: Arc<std::sync::Mutex<VecDeque<AssistantMessage>>> =
        Arc::new(std::sync::Mutex::new(VecDeque::from(vec![
            assistant_two_tool_calls(),
            AssistantMessage {
                role: ai::types::AssistantRole,
                content: vec![ai::types::Content::text("done")],
                api: ai::types::Api::Other("openai-responses".into()),
                provider: "mock".to_string(),
                model: "mock".to_string(),
                response_model: None,
                response_id: None,
                usage: ai::types::Usage::zero(),
                stop_reason: StopReason::Stop,
                deferred: None,
                error_message: None,
                raw_stop_reason: None,
                end_turn: None,
                timestamp: 0,
            },
        ])));
    agent::stream_fn(move |_model, _ctx, _opts| {
        let (mut prod, stream) = create_assistant_message_event_stream();
        let next = script.lock().expect("script lock").pop_front();
        let release = Arc::clone(&release);
        tokio::spawn(async move {
            let message = match next {
                Some(m) => m,
                None => {
                    let err = AssistantMessage::terminal(
                        ai::types::Api::Other("mock".into()),
                        "mock",
                        "mock",
                        StopReason::Error,
                        "No more mock responses queued",
                        0,
                    );
                    prod.push(AssistantMessageEvent::Error {
                        reason: ai::types::ErrorReason::Error,
                        error: err,
                    })
                    .await;
                    return;
                }
            };
            let is_tool_turn = matches!(message.stop_reason, StopReason::ToolUse);
            let partial = Arc::new(message.clone());
            prod.push(AssistantMessageEvent::Start {
                partial: partial.clone(),
            })
            .await;
            let reason = match message.stop_reason {
                StopReason::ToolUse => DoneReason::ToolUse,
                StopReason::Stop => DoneReason::Stop,
                _ => DoneReason::Stop,
            };
            prod.push(AssistantMessageEvent::Done { reason, message })
                .await;
            // If this was the tool-call turn, schedule the release of `tool-1`
            // after 20ms — mirrors TS `setTimeout(() => releaseFirst?.(), 20)`.
            if is_tool_turn {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                release.notify_one();
            }
        });
        stream
    })
}

fn assistant_two_tool_calls() -> AssistantMessage {
    AssistantMessage {
        role: ai::types::AssistantRole,
        content: vec![
            ai::types::Content::tool_call(
                "tool-1",
                "echo",
                serde_json::json!({ "value": "first" }),
            ),
            ai::types::Content::tool_call(
                "tool-2",
                "echo",
                serde_json::json!({ "value": "second" }),
            ),
        ],
        api: ai::types::Api::Other("openai-responses".into()),
        provider: "mock".to_string(),
        model: "mock".to_string(),
        response_model: None,
        response_id: None,
        usage: ai::types::Usage::zero(),
        stop_reason: StopReason::ToolUse,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    }
}

/// Extract tool_call_ids from `ToolExecutionEnd` events, in emit order.
fn tool_execution_end_ids(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect()
}

/// Extract tool_call_ids of tool-result `MessageEnd` events, in emit order.
fn tool_result_message_ids(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageEnd { message, .. } => match message {
                agent::AgentMessage::ToolResult(t) => Some(t.tool_call_id.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// Extract tool_call_ids from the `TurnEnd` payload's `tool_results`, in order.
fn turn_end_tool_result_ids(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TurnEnd { tool_results, .. } => Some(
                tool_results
                    .iter()
                    .map(|t| t.tool_call_id.clone())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect()
}

#[tokio::test]
async fn tool_execution_end_in_completion_order_results_in_source_order() {
    let coord = Coord {
        release: Arc::new(Notify::new()),
        first_resolved: Arc::new(AtomicBool::new(false)),
        parallel_observed: Arc::new(AtomicBool::new(false)),
    };
    let tool = EchoTool {
        schema: echo_schema(),
        coord: Coord {
            release: Arc::clone(&coord.release),
            first_resolved: Arc::clone(&coord.first_resolved),
            parallel_observed: Arc::clone(&coord.parallel_observed),
        },
    };
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![Arc::new(tool)],
    };

    let mut config = base_config();
    config.tool_execution = ToolExecutionMode::Parallel;

    let stream_fn = two_call_stream_fn(Arc::clone(&coord.release));
    let (events, _new_messages) =
        run_and_collect(vec![user_message("echo both")], context, config, stream_fn).await;

    // Genuine parallelism: tool-2 ran while tool-1 was blocked.
    assert!(
        coord.parallel_observed.load(Ordering::SeqCst),
        "tool-2 should have run while tool-1 was still pending"
    );

    // THE invariant: completion order for tool_execution_end ...
    assert_eq!(
        tool_execution_end_ids(&events),
        vec!["tool-2".to_string(), "tool-1".to_string()],
        "ToolExecutionEnd must be in completion order (tool-2 finishes first)"
    );
    // ... but source/ordinal order for tool-result MessageEnd + TurnEnd.
    assert_eq!(
        tool_result_message_ids(&events),
        vec!["tool-1".to_string(), "tool-2".to_string()],
        "tool-result MessageEnd must be in source/ordinal order"
    );
    assert_eq!(
        turn_end_tool_result_ids(&events),
        vec!["tool-1".to_string(), "tool-2".to_string()],
        "TurnEnd.toolResults must be in source/ordinal order"
    );
}

#[tokio::test]
async fn tool_parallelism_cap_prevents_overlap() {
    let coord = Coord {
        release: Arc::new(Notify::new()),
        first_resolved: Arc::new(AtomicBool::new(false)),
        parallel_observed: Arc::new(AtomicBool::new(false)),
    };
    let tool = EchoTool {
        schema: echo_schema(),
        coord: Coord {
            release: Arc::clone(&coord.release),
            first_resolved: Arc::clone(&coord.first_resolved),
            parallel_observed: Arc::clone(&coord.parallel_observed),
        },
    };
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: vec![Arc::new(tool)],
    };
    let mut config = base_config();
    config.tool_execution = ToolExecutionMode::Parallel;
    config.tool_parallelism = 1;

    let stream_fn = two_call_stream_fn(Arc::clone(&coord.release));
    let (events, _new_messages) =
        run_and_collect(vec![user_message("echo both")], context, config, stream_fn).await;

    assert!(
        !coord.parallel_observed.load(Ordering::SeqCst),
        "a cap of 1 must not run the second tool while the first is pending"
    );
    assert_eq!(
        tool_execution_end_ids(&events),
        vec!["tool-1".to_string(), "tool-2".to_string()]
    );
    assert_eq!(
        tool_result_message_ids(&events),
        vec!["tool-1".to_string(), "tool-2".to_string()]
    );
}

struct BarrierTool {
    schema: ai::types::Tool,
    exclusive: bool,
    first: Arc<tokio::sync::Barrier>,
    second: Arc<tokio::sync::Barrier>,
    active: Arc<std::sync::atomic::AtomicUsize>,
    exclusive_seen: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl agent::AgentTool for BarrierTool {
    fn schema(&self) -> &ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        &self.schema.name
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        if self.exclusive {
            ToolExecutionMode::Sequential
        } else {
            ToolExecutionMode::Parallel
        }
    }
    async fn execute(
        &self,
        id: &str,
        _: serde_json::Value,
        _: tokio_util::sync::CancellationToken,
        _: Arc<dyn Fn(agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<agent::AgentToolResult, agent::AgentError> {
        if self.exclusive {
            assert_eq!(self.active.load(Ordering::SeqCst), 0);
            self.exclusive_seen.store(true, Ordering::SeqCst);
        } else {
            if id.starts_with('c') {
                assert!(self.exclusive_seen.load(Ordering::SeqCst));
            }
            self.active.fetch_add(1, Ordering::SeqCst);
            if id.starts_with('a') {
                self.first.wait().await;
            } else {
                self.second.wait().await;
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(agent::AgentToolResult::text(id))
    }
}

#[tokio::test]
async fn exclusive_tool_separates_parallel_groups() {
    let first = Arc::new(tokio::sync::Barrier::new(2));
    let second = Arc::new(tokio::sync::Barrier::new(2));
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let exclusive_seen = Arc::new(AtomicBool::new(false));
    let tool = |name: &str, exclusive| {
        Arc::new(BarrierTool {
            schema: ai::types::Tool {
                name: name.into(),
                description: String::new(),
                parameters: ai::types::Schema::new(serde_json::json!({"type":"object"})),
                constrained_sampling: None,
            },
            exclusive,
            first: first.clone(),
            second: second.clone(),
            active: active.clone(),
            exclusive_seen: exclusive_seen.clone(),
        }) as Arc<dyn agent::AgentTool>
    };
    let context = AgentContext {
        tools: vec![tool("read", false), tool("exclusive", true)],
        ..Default::default()
    };
    let assistant = common::assistant_tool_calls(
        vec![
            ("a1", "read", serde_json::json!({})),
            ("a2", "read", serde_json::json!({})),
            ("b", "exclusive", serde_json::json!({})),
            ("c1", "read", serde_json::json!({})),
            ("c2", "read", serde_json::json!({})),
        ],
        StopReason::ToolUse,
    );
    let stream = common::mock_stream_fn(vec![
        assistant,
        common::assistant_text("done", StopReason::Stop),
    ]);
    let snapshots = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = snapshots.clone();
    let mut config = base_config();
    config.after_tool_call = Some(Arc::new(move |ctx, _| {
        observed
            .lock()
            .unwrap()
            .push(ctx.context as *const AgentContext as usize);
        Box::pin(async { None })
    }));
    let (events, _) = run_and_collect(vec![user_message("run")], context, config, stream).await;
    assert_eq!(
        tool_result_message_ids(&events),
        ["a1", "a2", "b", "c1", "c2"]
    );
    let snapshots = snapshots.lock().unwrap();
    assert_eq!(snapshots.len(), 5);
    assert!(snapshots.iter().all(|snapshot| *snapshot == snapshots[0]));
}
