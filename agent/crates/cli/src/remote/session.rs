//! Client-side session model: folds the server's event stream into a
//! render-friendly transcript plus a small amount of session state.
//!
//! This is deliberately independent of any local agent harness — it is the
//! client analogue of the TS `packages/coding-agent/src/client/` remote-session
//! layer, built on rpi's own protocol types.

use serde_json::Value;

use crate::remote::protocol::{RemoteEvent, RemoteResponse, SessionState};

/// Re-exported from the shared render layer so the wire `Value` parsing lives in
/// exactly one place (the session fold and the transcript view both use it).
pub use crate::transcript_view::tool_result_text;

/// A tool invocation as rendered in the transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolItem {
    pub id: String,
    pub name: String,
    pub args: Value,
    /// Latest partial output reported via `on_update`, if any.
    pub partial: Option<String>,
    /// Final output once the tool finished.
    pub result: Option<String>,
    pub is_error: bool,
    pub done: bool,
}

/// One item in the remote transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptItem {
    /// A prompt the local user submitted.
    User(String),
    /// Assistant response text (grows as deltas arrive).
    Assistant(String),
    /// Assistant reasoning text (grows as deltas arrive).
    Thinking(String),
    /// A tool call with its (partial/final) output.
    Tool(ToolItem),
    /// A local notice (retry, error, connection status).
    Notice(String),
}

/// The transcript the UI renders, in order.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    pub items: Vec<TranscriptItem>,
}

impl Transcript {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.items.push(TranscriptItem::User(text.into()));
    }

    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.items.push(TranscriptItem::Notice(text.into()));
    }

    fn push_assistant(&mut self) -> usize {
        self.items.push(TranscriptItem::Assistant(String::new()));
        self.items.len() - 1
    }

    fn push_thinking(&mut self) -> usize {
        self.items.push(TranscriptItem::Thinking(String::new()));
        self.items.len() - 1
    }

    /// Index of the last item of a given variant kind.
    #[allow(dead_code)]
    fn last_index<F: Fn(&TranscriptItem) -> bool>(&self, pred: F) -> Option<usize> {
        self.items.iter().rposition(pred)
    }

    /// Find a tool item by call id.
    pub fn tool_mut(&mut self, id: &str) -> Option<&mut ToolItem> {
        self.items.iter_mut().find_map(|item| match item {
            TranscriptItem::Tool(tool) if tool.id == id => Some(tool),
            _ => None,
        })
    }

    /// Plain-text projection (used for logs, tests, and non-TUI output).
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for item in &self.items {
            match item {
                TranscriptItem::User(text) => {
                    out.push_str("> ");
                    out.push_str(text);
                    out.push('\n');
                }
                TranscriptItem::Assistant(text) => {
                    out.push_str(text);
                    out.push('\n');
                }
                TranscriptItem::Thinking(text) => {
                    out.push_str("[thinking] ");
                    out.push_str(text);
                    out.push('\n');
                }
                TranscriptItem::Tool(tool) => {
                    let status = if tool.done {
                        if tool.is_error {
                            "error"
                        } else {
                            "ok"
                        }
                    } else {
                        "running"
                    };
                    out.push_str(&format!("[tool {} {}]\n", tool.name, status));
                    if let Some(body) = tool.result.as_ref().or(tool.partial.as_ref()) {
                        out.push_str(body);
                        out.push('\n');
                    }
                }
                TranscriptItem::Notice(text) => {
                    out.push('[');
                    out.push_str(text);
                    out.push_str("]\n");
                }
            }
        }
        out
    }
}

/// Extract concatenated text from a serialized `AgentMessage`/`AssistantMessage`
/// (`content: [{type:"text",text:…}]`).
pub fn message_text(message: &Value) -> String {
    let mut out = String::new();
    if let Some(content) = message.get("content") {
        if let Some(text) = content.as_str() {
            return text.to_string();
        }
        if let Some(blocks) = content.as_array() {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        out.push_str(text);
                    }
                }
            }
        }
    }
    out
}

/// The logical role of a serialized message (`kind` or `role`).
pub fn message_role(message: &Value) -> &str {
    message
        .get("kind")
        .or_else(|| message.get("role"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// Text out of a wire tool-result payload (`{content:[{type:"text",text}]}`).

/// Client-side session state: the transcript plus the latest server state.
#[derive(Debug, Clone, Default)]
pub struct RemoteSession {
    pub transcript: Transcript,
    pub state: SessionState,
    /// True between `agent_start` and `agent_end`.
    pub streaming: bool,
    /// Set once the server sent its `ready` line.
    pub ready: bool,
    /// Most recent error surfaced locally or by the server.
    pub last_error: Option<String>,
    /// Number of retries currently being waited on (0 when not retrying).
    pub retry_attempt: Option<(u32, u32)>,
    /// Delay of the pending retry in milliseconds (drives the status countdown).
    pub retry_delay_ms: Option<u64>,
    open_assistant: Option<usize>,
    open_thinking: Option<usize>,
}

impl RemoteSession {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a locally-submitted prompt so it renders immediately.
    pub fn on_user_prompt(&mut self, text: impl Into<String>) {
        self.transcript.push_user(text);
        self.streaming = true;
        self.open_assistant = None;
        self.open_thinking = None;
    }

    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.transcript.push_notice(text);
    }

    /// Apply a raw child line (`ready` / event / response) as forwarded by the
    /// server's `event` notifications.
    pub fn apply_line(&mut self, line: &Value) {
        match line.get("type").and_then(Value::as_str) {
            Some("ready") => self.ready = true,
            Some("response") => {
                if let Ok(response) = serde_json::from_value::<RemoteResponse>(line.clone()) {
                    self.apply_response(&response);
                }
            }
            Some(_) => {
                if let Ok(event) = serde_json::from_value::<RemoteEvent>(line.clone()) {
                    self.apply_event(&event);
                }
            }
            None => {}
        }
    }

    /// Apply a decoded response (e.g. to a `get_state` command).
    pub fn apply_response(&mut self, response: &RemoteResponse) {
        if !response.is_ok() {
            if let Some(error) = &response.error {
                self.last_error = Some(error.clone());
                self.transcript.push_notice(format!("error: {error}"));
            }
            return;
        }
        let Some(result) = &response.result else {
            return;
        };
        if result.get("sessionId").is_some() && result.get("activeTools").is_some() {
            if let Ok(state) = serde_json::from_value::<SessionState>(result.clone()) {
                self.state = state;
            }
        }
    }

    /// Fold one lifecycle event into the transcript.
    pub fn apply_event(&mut self, event: &RemoteEvent) {
        match event {
            RemoteEvent::UiDialogRequest { .. } | RemoteEvent::UiDialogClosed { .. } => {}
            RemoteEvent::AgentStart => {
                self.streaming = true;
                self.retry_attempt = None;
                self.retry_delay_ms = None;
                self.open_assistant = None;
                self.open_thinking = None;
            }
            RemoteEvent::AgentEnd { .. } => {
                self.streaming = false;
                self.retry_attempt = None;
                self.retry_delay_ms = None;
                self.open_assistant = None;
                self.open_thinking = None;
            }
            RemoteEvent::RetryScheduled {
                attempt,
                max_retries,
                delay_ms,
                error,
            } => {
                self.retry_attempt = Some((*attempt, *max_retries));
                self.retry_delay_ms = Some(*delay_ms);
                self.transcript.push_notice(format!(
                    "retry {attempt}/{max_retries} in {delay_ms}ms: {error}"
                ));
            }
            RemoteEvent::TurnStart | RemoteEvent::TurnEnd { .. } => {}
            RemoteEvent::MessageStart { message, .. } => {
                if message_role(message) == "assistant" {
                    // If the previous assistant item is still open with no
                    // content, reuse it; otherwise start a new block.
                    self.open_assistant = Some(self.transcript.push_assistant());
                    self.open_thinking = None;
                }
            }
            RemoteEvent::MessageEnd { message, .. } => {
                if message_role(message) == "assistant" {
                    let text = message_text(message);
                    if let Some(index) = self.open_assistant.take() {
                        if let Some(TranscriptItem::Assistant(existing)) =
                            self.transcript.items.get_mut(index)
                        {
                            if !text.is_empty() {
                                *existing = text;
                            }
                        }
                    } else if !text.is_empty() {
                        self.transcript.items.push(TranscriptItem::Assistant(text));
                    }
                }
                self.open_thinking = None;
            }
            RemoteEvent::MessageUpdate {
                event_type,
                delta,
                content_index,
                ..
            } => {
                let Some(delta) = delta else { return };
                if delta.is_empty() {
                    return;
                }
                match event_type.as_str() {
                    "text_delta" => {
                        let index = match self.open_assistant {
                            Some(index) => index,
                            None => {
                                let index = self.transcript.push_assistant();
                                self.open_assistant = Some(index);
                                index
                            }
                        };
                        if let Some(TranscriptItem::Assistant(text)) =
                            self.transcript.items.get_mut(index)
                        {
                            text.push_str(delta);
                        }
                    }
                    "thinking_delta" => {
                        let index = match self.open_thinking {
                            Some(index) => index,
                            None => {
                                let index = self.transcript.push_thinking();
                                self.open_thinking = Some(index);
                                index
                            }
                        };
                        if let Some(TranscriptItem::Thinking(text)) =
                            self.transcript.items.get_mut(index)
                        {
                            text.push_str(delta);
                        }
                    }
                    "tool_call_delta" => {
                        let _ = content_index;
                    }
                    _ => {}
                }
            }
            RemoteEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                self.transcript.items.push(TranscriptItem::Tool(ToolItem {
                    id: tool_call_id.clone(),
                    name: tool_name.clone(),
                    args: args.clone(),
                    partial: None,
                    result: None,
                    is_error: false,
                    done: false,
                }));
            }
            RemoteEvent::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
                ..
            } => {
                let text = tool_result_text(partial_result);
                if let Some(tool) = self.transcript.tool_mut(tool_call_id) {
                    tool.partial = Some(text);
                }
            }
            RemoteEvent::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
                ..
            } => {
                let text = tool_result_text(result);
                if let Some(tool) = self.transcript.tool_mut(tool_call_id) {
                    tool.result = Some(text);
                    tool.is_error = *is_error;
                    tool.done = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(value: serde_json::Value) -> RemoteEvent {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn folds_text_stream_into_one_assistant_block() {
        let mut session = RemoteSession::new();
        session.on_user_prompt("hi");
        session.apply_event(&event(json!({"type": "agent_start"})));
        session.apply_event(&event(json!({
            "type": "message_start",
            "message": {"kind": "assistant", "content": []}
        })));
        for delta in ["Hel", "lo", "!"] {
            session.apply_event(&event(json!({
                "type": "message_update",
                "message": {"kind": "assistant", "content": []},
                "assistantMessageEvent": {"type": "text_delta"},
                "eventType": "text_delta",
                "contentIndex": 0,
                "delta": delta,
            })));
        }
        session.apply_event(&event(json!({
            "type": "message_end",
            "message": {"kind": "assistant", "content": [{"type": "text", "text": "Hello!"}]}
        })));
        session.apply_event(&event(
            json!({"type": "agent_end", "messageCount": 1, "messages": []}),
        ));

        assert!(!session.streaming);
        assert_eq!(session.transcript.items.len(), 2); // user + assistant
        assert_eq!(
            session.transcript.items[1],
            TranscriptItem::Assistant("Hello!".into())
        );
    }

    #[test]
    fn tracks_tool_lifecycle_and_errors() {
        let mut session = RemoteSession::new();
        session.apply_event(&event(json!({
            "type": "tool_execution_start",
            "toolCallId": "c1",
            "toolName": "read",
            "args": {"path": "a.txt"}
        })));
        session.apply_event(&event(json!({
            "type": "tool_execution_end",
            "toolCallId": "c1",
            "toolName": "read",
            "isError": true,
            "result": {"content": [{"type": "text", "text": "missing"}]}
        })));

        match &session.transcript.items[0] {
            TranscriptItem::Tool(tool) => {
                assert_eq!(tool.name, "read");
                assert!(tool.done);
                assert!(tool.is_error);
                assert_eq!(tool.result.as_deref(), Some("missing"));
            }
            other => panic!("expected tool, got {other:?}"),
        }
    }

    #[test]
    fn thinking_deltas_get_their_own_block() {
        let mut session = RemoteSession::new();
        session.apply_event(&event(json!({
            "type": "message_update",
            "assistantMessageEvent": {"type": "thinking_delta"},
            "eventType": "thinking_delta",
            "delta": "why",
        })));
        assert_eq!(
            session.transcript.items[0],
            TranscriptItem::Thinking("why".into())
        );
    }

    #[test]
    fn ready_and_error_lines_update_state() {
        let mut session = RemoteSession::new();
        session.apply_line(&json!({"type": "ready"}));
        assert!(session.ready);
        session.apply_line(&json!({
            "type": "response", "id": "1", "status": "error", "error": "boom"
        }));
        assert_eq!(session.last_error.as_deref(), Some("boom"));
        session.apply_line(&json!({
            "type": "response", "id": "2", "status": "ok",
            "result": {"model": "m", "thinkingLevel": "off",
                       "activeTools": ["read"], "sessionId": "s1"}
        }));
        assert_eq!(session.state.model.as_deref(), Some("m"));
        assert_eq!(session.state.active_tools, vec!["read".to_string()]);
    }
}
