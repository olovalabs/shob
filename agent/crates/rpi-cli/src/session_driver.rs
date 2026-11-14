//! Transport-agnostic **session driver** — the "only the transport differs"
//! boundary.
//!
//! A driver bundles the two things a TUI needs from its backend:
//!
//! 1. a **stream** of normalized events ([`DriverEvent`] / [`UiEvent`]), and
//! 2. a **command** sink ([`DriverCommand`]).
//!
//! The remote client ([`RemoteDriver`]) implements this over TCP JSON-RPC. The
//! local interactive host drives the same rendering path via
//! [`map_agent_event`] (its own in-process `AgentEvent` stream → [`UiEvent`]).
//! Because both sides speak [`UiEvent`], they render through the same
//! [`crate::transcript_view::TranscriptView`].
//!
//! [`UiEvent`]: crate::transcript_view::UiEvent

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};

use rpi_agent::{AgentEvent, AgentMessage};
use rpi_ai::types::{AssistantMessage, Content, ThinkingLevel};
use rpi_harness::agent_harness::AgentLane;
use rpi_tui::AssistantBlock;

use crate::remote::client::RemoteClient;
use crate::remote::protocol::{RemoteCommand, RemoteEvent};
use crate::remote::session::RemoteSession;
use crate::transcript_view::{UiEvent, UiToolCall};

/// A command the TUI sends to its backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverCommand {
    /// Answer or cancel a remote question by request id.
    UiDialogResponse {
        request_id: String,
        answer: Option<Value>,
    },
    /// Run a prompt on the backend's main lane.
    Prompt(String),
    /// Interrupt the in-flight run.
    Abort,
    /// Re-query the backend's session state (model / thinking / tools).
    RefreshState,
    /// Switch the active model by id.
    SetModel(String),
    /// Set the thinking level (`off`…`max`).
    SetThinkingLevel(String),
    /// Replace the active tool set.
    SetActiveTools(Vec<String>),
}

/// A snapshot of backend session state for the chrome (header / footer / retry).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DriverStatus {
    pub ready: bool,
    pub streaming: bool,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub active_tools: Vec<String>,
    /// `(attempt, max, delay_ms)` while a retry is pending.
    pub retry: Option<(u32, u32, u64)>,
}

/// An item pushed from a driver to the TUI.
#[derive(Debug, Clone)]
pub enum DriverEvent {
    UiDialog(rpi_extensions::UiDialogRequest),
    UiDialogClosed(String),
    /// A transcript event.
    Ui(UiEvent),
    /// A fresh session-state snapshot.
    Status(DriverStatus),
    /// A local notice line (error / connection message).
    Notice(String),
    /// The backend stream ended.
    Ended,
}

/// A backend the TUI can drive: it yields [`DriverEvent`]s and accepts
/// [`DriverCommand`]s. Both the in-process host and the remote client
/// implement it, so the transcript render layer above is transport-agnostic.
#[async_trait]
pub trait SessionDriver: Send + Sync {
    /// Take the driver's event receiver. Returns `Some` exactly once; later
    /// calls return `None`.
    fn events(&self) -> Option<mpsc::UnboundedReceiver<DriverEvent>>;

    /// Send a command to the backend.
    async fn send(&self, command: DriverCommand) -> Result<(), String>;
}

// ---------------------------------------------------------------------------
// Local projection: in-process `AgentEvent` → normalized `UiEvent`
// ---------------------------------------------------------------------------

/// Project an in-process [`AgentEvent`] into the shared [`UiEvent`]. Returns
/// `None` for events with no transcript effect.
pub fn map_agent_event(event: &AgentEvent) -> Option<UiEvent> {
    match event {
        AgentEvent::AgentStart => Some(UiEvent::AgentStart),
        AgentEvent::AgentEnd { .. } => Some(UiEvent::AgentEnd),
        AgentEvent::RetryScheduled {
            attempt,
            max_retries,
            delay_ms,
            error,
        } => Some(UiEvent::Retry {
            attempt: *attempt,
            max_retries: *max_retries,
            delay_ms: *delay_ms,
            error: error.clone(),
        }),
        AgentEvent::TurnStart | AgentEvent::TurnEnd { .. } => None,
        AgentEvent::MessageStart { message, .. } => match message {
            AgentMessage::Assistant(a) => Some(UiEvent::AssistantStart {
                blocks: assistant_blocks(a),
            }),
            _ => None,
        },
        AgentEvent::MessageUpdate { message, .. } => Some(UiEvent::AssistantUpdate {
            blocks: assistant_blocks(message),
            tool_calls: assistant_tool_calls(message),
        }),
        AgentEvent::MessageEnd { message, .. } => match message {
            AgentMessage::Assistant(a) => Some(UiEvent::AssistantEnd {
                blocks: assistant_blocks(a),
            }),
            _ => None,
        },
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => Some(UiEvent::ToolStart {
            id: tool_call_id.clone(),
            name: tool_name.clone(),
            args: args.clone(),
        }),
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            partial_result,
        } => Some(UiEvent::ToolUpdate {
            id: tool_call_id.clone(),
            name: tool_name.clone(),
            args: args.clone(),
            result: crate::remote::protocol::tool_result_json(partial_result),
        }),
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => Some(UiEvent::ToolEnd {
            id: tool_call_id.clone(),
            name: tool_name.clone(),
            result: crate::remote::protocol::tool_result_json(result),
            is_error: *is_error,
        }),
    }
}

/// Provider-free assistant block projection (text / thinking / decoded image),
/// in document order.
pub fn assistant_blocks(msg: &AssistantMessage) -> Vec<AssistantBlock> {
    msg.content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(AssistantBlock::Text(t.text.clone())),
            Content::Thinking(t) => Some(AssistantBlock::Thinking(t.thinking.clone())),
            Content::Image(image) => base64::engine::general_purpose::STANDARD
                .decode(&*image.data)
                .ok()
                .filter(|data| !data.is_empty())
                .map(AssistantBlock::Image),
            _ => None,
        })
        .collect()
}

/// Finalized tool calls in an assistant message (placeholder calls with an
/// empty name are skipped).
pub fn assistant_tool_calls(msg: &AssistantMessage) -> Vec<UiToolCall> {
    msg.content
        .iter()
        .filter_map(|c| match c {
            Content::ToolCall(tc) if !tc.name.trim().is_empty() => Some(UiToolCall {
                id: tc.id.clone(),
                name: tc.name.clone(),
                args: tc.arguments.clone(),
            }),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Remote driver
// ---------------------------------------------------------------------------

/// A [`SessionDriver`] backed by a remote `rpi --server` over TCP JSON-RPC.
pub struct RemoteDriver {
    client: tokio::sync::Mutex<RemoteClient>,
    session_id: String,
    next_id: AtomicU64,
    events: StdMutex<Option<mpsc::UnboundedReceiver<DriverEvent>>>,
}

impl RemoteDriver {
    /// Connect, authenticate, start a remote session, subscribe, and spawn the
    /// event pump that folds the wire stream into [`DriverEvent`]s.
    pub async fn connect(addr: &str, token: Option<&str>) -> Result<Self, String> {
        let mut client = RemoteClient::connect(addr).await?;
        client
            .authenticate(token.unwrap_or(""))
            .await
            .map_err(auth_hint)?;

        let started = client
            .call("start_session", json!({}))
            .await
            .map_err(auth_hint)?;
        let session_id = started
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or("server did not return a sessionId")?
            .to_string();

        let subscribed = client
            .call("subscribe", json!({"sessionId": session_id}))
            .await?;
        let subscription_id = subscribed
            .get("subscriptionId")
            .and_then(Value::as_u64)
            .ok_or("server did not return a subscriptionId")?;

        let mut lines = client.start_event_pump(subscription_id);
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut session = RemoteSession::new();
            let mut mapper = RemoteEventMapper::default();
            let mut last_error: Option<String> = None;
            while let Some(line) = lines.recv().await {
                session.apply_line(&line);
                let _ = tx.send(DriverEvent::Status(status_of(&session)));
                if session.last_error != last_error {
                    last_error = session.last_error.clone();
                    if let Some(error) = &last_error {
                        let _ = tx.send(DriverEvent::Notice(format!("error: {error}")));
                    }
                }
                if let Ok(event) = serde_json::from_value::<RemoteEvent>(line) {
                    if let Some(event) = mapper.map(&event) {
                        let _ = tx.send(event);
                    }
                }
            }
            let _ = tx.send(DriverEvent::Ended);
        });

        let driver = Self {
            client: tokio::sync::Mutex::new(client),
            session_id,
            next_id: AtomicU64::new(1),
            events: StdMutex::new(Some(rx)),
        };
        // Seed the state (model / thinking / tools) for the chrome.
        let _ = driver.send(DriverCommand::RefreshState).await;
        Ok(driver)
    }

    /// Best-effort session teardown.
    pub async fn shutdown(&self) {
        let mut client = self.client.lock().await;
        client.stop_session(&self.session_id).await;
    }
}

#[async_trait]
impl SessionDriver for RemoteDriver {
    fn events(&self) -> Option<mpsc::UnboundedReceiver<DriverEvent>> {
        self.events.lock().unwrap().take()
    }

    async fn send(&self, command: DriverCommand) -> Result<(), String> {
        let remote = match command {
            DriverCommand::UiDialogResponse { request_id, answer } => {
                RemoteCommand::UiDialogResponse {
                    request_id,
                    cancelled: answer.is_none(),
                    answer,
                }
            }
            DriverCommand::Prompt(content) => RemoteCommand::Prompt { content },
            DriverCommand::Abort => RemoteCommand::Abort,
            DriverCommand::RefreshState => RemoteCommand::GetState,
            DriverCommand::SetModel(model) => RemoteCommand::SetModel { model },
            DriverCommand::SetThinkingLevel(level) => RemoteCommand::SetThinkingLevel { level },
            DriverCommand::SetActiveTools(tools) => RemoteCommand::SetActiveTools { tools },
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut client = self.client.lock().await;
        client
            .send_command(&self.session_id, remote.with_id(id))
            .await
    }
}

/// Snapshot the foldable parts of a [`RemoteSession`] the chrome needs.
fn status_of(session: &RemoteSession) -> DriverStatus {
    DriverStatus {
        ready: session.ready,
        streaming: session.streaming,
        model: session.state.model.clone(),
        thinking: session
            .state
            .thinking_level
            .as_ref()
            .and_then(Value::as_str)
            .map(str::to_string),
        active_tools: session.state.active_tools.clone(),
        retry: session
            .retry_attempt
            .map(|(attempt, max)| (attempt, max, session.retry_delay_ms.unwrap_or(1000))),
    }
}

/// Stateful wire-to-UI adapter used by the actual remote event pump. Keep the
/// stateless projection below available for callers with complete snapshots.
#[derive(Default)]
struct RemoteEventMapper {
    messages: crate::remote::message_accumulator::MessageAccumulator,
}

impl RemoteEventMapper {
    fn map(&mut self, event: &RemoteEvent) -> Option<DriverEvent> {
        map_remote_driver_event(&self.messages.apply(event))
    }
}

/// Preserve dialog messages alongside transcript events; they are not part
/// of the transcript and must reach the shell's modal input slot.
fn map_remote_driver_event(event: &RemoteEvent) -> Option<DriverEvent> {
    match event {
        RemoteEvent::UiDialogRequest {
            request_id,
            tool_call_id,
            ui,
        } => Some(DriverEvent::UiDialog(rpi_extensions::UiDialogRequest {
            request_id: request_id.clone(),
            tool_call_id: tool_call_id.clone(),
            ui: ui.clone(),
            raw: serde_json::to_value(event).unwrap_or(Value::Null),
        })),
        RemoteEvent::UiDialogClosed { request_id } => {
            Some(DriverEvent::UiDialogClosed(request_id.clone()))
        }
        _ => map_remote_event(event).map(DriverEvent::Ui),
    }
}

/// Project a wire [`RemoteEvent`] into the shared [`UiEvent`].
pub fn map_remote_event(event: &RemoteEvent) -> Option<UiEvent> {
    use crate::remote::session::message_role;
    use crate::transcript_view::{assistant_blocks_from_value, tool_calls_from_value};

    match event {
        RemoteEvent::UiDialogRequest { .. } | RemoteEvent::UiDialogClosed { .. } => None,
        RemoteEvent::AgentStart => Some(UiEvent::AgentStart),
        RemoteEvent::AgentEnd { .. } => Some(UiEvent::AgentEnd),
        RemoteEvent::RetryScheduled {
            attempt,
            max_retries,
            delay_ms,
            error,
        } => Some(UiEvent::Retry {
            attempt: *attempt,
            max_retries: *max_retries,
            delay_ms: *delay_ms,
            error: error.clone(),
        }),
        RemoteEvent::MessageStart { message, .. } => {
            if message_role(message) != "assistant" {
                return None;
            }
            Some(UiEvent::AssistantStart {
                blocks: assistant_blocks_from_value(message),
            })
        }
        RemoteEvent::MessageUpdate { message, .. } => {
            if !message.is_object() {
                return None;
            }
            Some(UiEvent::AssistantUpdate {
                blocks: assistant_blocks_from_value(message),
                tool_calls: tool_calls_from_value(message),
            })
        }
        RemoteEvent::MessageEnd { message, .. } => {
            if message_role(message) != "assistant" {
                return None;
            }
            Some(UiEvent::AssistantEnd {
                blocks: assistant_blocks_from_value(message),
            })
        }
        RemoteEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => Some(UiEvent::ToolStart {
            id: tool_call_id.clone(),
            name: tool_name.clone(),
            args: args.clone(),
        }),
        RemoteEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            partial_result,
        } => Some(UiEvent::ToolUpdate {
            id: tool_call_id.clone(),
            name: tool_name.clone(),
            args: args.clone(),
            result: partial_result.clone(),
        }),
        RemoteEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => Some(UiEvent::ToolEnd {
            id: tool_call_id.clone(),
            name: tool_name.clone(),
            result: result.clone(),
            is_error: *is_error,
        }),
        RemoteEvent::TurnStart | RemoteEvent::TurnEnd { .. } => None,
    }
}

/// Append a `--token` hint when the server rejected us for authentication.
fn auth_hint(error: String) -> String {
    let lower = error.to_lowercase();
    if lower.contains("authentication") || lower.contains("token") {
        format!("{error} (retrieve the token printed by `rpi --server` and pass `--token <token>`)")
    } else {
        error
    }
}

// ---------------------------------------------------------------------------
// Local driver
// ---------------------------------------------------------------------------

/// A [`SessionDriver`] backed by an in-process harness lane: it turns the
/// lane's `broadcast::Receiver<AgentEvent>` into [`DriverEvent`]s and maps
/// [`DriverCommand`]s onto the lane. Additive — the interactive TUI can adopt
/// it without changing the rich local operations it performs through the lane
/// directly.
///
/// Model switching is **not** exposed through the driver: `AgentLane::set_model`
/// takes a resolved `Model`, while the driver only carries an id. The local TUI
/// keeps using its own model catalog for `/model`.
pub struct LocalDriver {
    lane: Arc<dyn AgentLane>,
    status_tx: mpsc::UnboundedSender<DriverEvent>,
    events: StdMutex<Option<mpsc::UnboundedReceiver<DriverEvent>>>,
}

impl LocalDriver {
    /// Wrap a lane plus its live event receiver.
    pub fn new(lane: Arc<dyn AgentLane>, events: broadcast::Receiver<AgentEvent>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let status_tx = tx.clone();
        tokio::spawn(pump_agent_events(lane.clone(), events, tx));
        Self {
            lane,
            status_tx,
            events: StdMutex::new(Some(rx)),
        }
    }
}

#[async_trait]
impl SessionDriver for LocalDriver {
    fn events(&self) -> Option<mpsc::UnboundedReceiver<DriverEvent>> {
        self.events.lock().unwrap().take()
    }

    async fn send(&self, command: DriverCommand) -> Result<(), String> {
        match command {
            DriverCommand::UiDialogResponse { .. } => {
                Err("local driver has no dialog mailbox".to_string())
            }
            DriverCommand::Prompt(text) => {
                // Drive the run on a background task: `prompt_text` only returns
                // once the run completes, but a driver `send` must not block the
                // TUI loop on the whole agent turn.
                let lane = self.lane.clone();
                let tx = self.status_tx.clone();
                tokio::spawn(async move {
                    if let Err(error) = lane.prompt_text(&text, Vec::new()).await {
                        let _ = tx.send(DriverEvent::Notice(format!("error: {error}")));
                    }
                });
                Ok(())
            }
            DriverCommand::Abort => self
                .lane
                .abort()
                .await
                .map(|_| ())
                .map_err(|e| e.to_string()),
            DriverCommand::RefreshState => {
                let status = query_status(&self.lane, false).await;
                let _ = self.status_tx.send(DriverEvent::Status(status));
                Ok(())
            }
            DriverCommand::SetThinkingLevel(level) => {
                let parsed = parse_thinking_level_name(&level)
                    .ok_or_else(|| format!("unknown thinking level: {level}"))?;
                self.lane
                    .set_thinking_level(parsed)
                    .await
                    .map_err(|e| e.to_string())
            }
            DriverCommand::SetActiveTools(tools) => self
                .lane
                .set_active_tools(tools)
                .await
                .map_err(|e| e.to_string()),
            DriverCommand::SetModel(_) => Err(
                "model switching is not available through the local driver (use /model)"
                    .to_string(),
            ),
        }
    }
}

/// Forward the lane's `AgentEvent` stream as [`DriverEvent`]s, emitting a fresh
/// [`DriverStatus`] on each run boundary.
async fn pump_agent_events(
    lane: Arc<dyn AgentLane>,
    mut rx: broadcast::Receiver<AgentEvent>,
    tx: mpsc::UnboundedSender<DriverEvent>,
) {
    let mut streaming = false;
    loop {
        match rx.recv().await {
            Ok(event) => {
                match &event {
                    AgentEvent::AgentStart => streaming = true,
                    AgentEvent::AgentEnd { .. } => streaming = false,
                    _ => {}
                }
                if let Some(ui) = map_agent_event(&event) {
                    if tx.send(DriverEvent::Ui(ui)).is_err() {
                        return;
                    }
                }
                if matches!(
                    &event,
                    AgentEvent::AgentStart
                        | AgentEvent::AgentEnd { .. }
                        | AgentEvent::RetryScheduled { .. }
                ) {
                    let status = query_status(&lane, streaming).await;
                    if tx.send(DriverEvent::Status(status)).is_err() {
                        return;
                    }
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    let _ = tx.send(DriverEvent::Ended);
}

/// Query the lane for a status snapshot.
async fn query_status(lane: &Arc<dyn AgentLane>, streaming: bool) -> DriverStatus {
    DriverStatus {
        ready: true,
        streaming,
        model: lane.get_model().await.ok().map(|model| model.id),
        thinking: lane
            .get_thinking_level()
            .await
            .ok()
            .map(thinking_level_name)
            .map(str::to_string),
        active_tools: lane.get_active_tools().await.unwrap_or_default(),
        retry: None,
    }
}

/// The wire/display name of a thinking level.
fn thinking_level_name(level: ThinkingLevel) -> &'static str {
    use ThinkingLevel::*;
    match level {
        Off => "off",
        Minimal => "minimal",
        Low => "low",
        Medium => "medium",
        High => "high",
        Xhigh => "xhigh",
        Max => "max",
    }
}

/// Parse a thinking-level name (case-insensitive); `None` for an unknown name.
fn parse_thinking_level_name(name: &str) -> Option<ThinkingLevel> {
    match name.trim().to_ascii_lowercase().as_str() {
        "off" => Some(ThinkingLevel::Off),
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::Xhigh),
        "max" => Some(ThinkingLevel::Max),
        _ => None,
    }
}

#[cfg(test)]
#[path = "remote/compatibility_tests.rs"]
mod compact_compatibility_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn remote_dialogs_are_forwarded_as_modal_events() {
        let request: RemoteEvent = serde_json::from_value(json!({
            "type":"ui_dialog_request", "requestId":"q1", "toolCallId":"c1", "ui":{"question":"Allow?"},
        })).unwrap();
        let Some(DriverEvent::UiDialog(request)) = map_remote_driver_event(&request) else {
            panic!("dialog was dropped")
        };
        assert_eq!(request.request_id, "q1");
        assert_eq!(request.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(request.ui["question"], "Allow?");
        assert!(
            matches!(map_remote_driver_event(&RemoteEvent::UiDialogClosed { request_id: "q1".into() }),
            Some(DriverEvent::UiDialogClosed(id)) if id == "q1")
        );
    }

    #[test]
    fn map_remote_event_ignores_non_assistant_messages() {
        let event = serde_json::from_value::<RemoteEvent>(json!({
            "type": "message_start",
            "message": {"kind": "user", "content": []}
        }))
        .unwrap();
        assert!(map_remote_event(&event).is_none());
    }

    #[test]
    fn map_remote_event_projects_assistant_update() {
        let event = serde_json::from_value::<RemoteEvent>(json!({
            "type": "message_update",
            "message": {"kind": "assistant", "content": [{"type": "text", "text": "hi"}]}
        }))
        .unwrap();
        match map_remote_event(&event) {
            Some(UiEvent::AssistantUpdate { blocks, tool_calls }) => {
                assert_eq!(blocks, vec![AssistantBlock::Text("hi".into())]);
                assert!(tool_calls.is_empty());
            }
            other => panic!("expected AssistantUpdate, got {other:?}"),
        }
    }

    #[test]
    fn map_remote_event_projects_tool_end() {
        let event = serde_json::from_value::<RemoteEvent>(json!({
            "type": "tool_execution_end",
            "toolCallId": "c1",
            "toolName": "read",
            "isError": true,
            "result": {"content": [{"type": "text", "text": "nope"}]}
        }))
        .unwrap();
        match map_remote_event(&event) {
            Some(UiEvent::ToolEnd {
                id, name, is_error, ..
            }) => {
                assert_eq!(id, "c1");
                assert_eq!(name, "read");
                assert!(is_error);
            }
            other => panic!("expected ToolEnd, got {other:?}"),
        }
    }

    #[test]
    fn auth_hint_mentions_the_token_flag() {
        assert!(auth_hint("authentication required".to_string()).contains("--token"));
        assert_eq!(auth_hint("boom".to_string()), "boom");
    }

    #[test]
    fn thinking_level_name_round_trips() {
        for name in ["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
            let level = parse_thinking_level_name(name).unwrap();
            assert_eq!(thinking_level_name(level), name);
        }
        assert_eq!(parse_thinking_level_name("HIGH"), Some(ThinkingLevel::High));
        assert_eq!(parse_thinking_level_name("bogus"), None);
    }
}
