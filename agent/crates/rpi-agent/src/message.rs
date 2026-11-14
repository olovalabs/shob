//! Mirrors `packages/agent/src/types.ts` (open enum portion) — the
//! `AgentMessage = Message | Custom` declaration-merging model.
//!
//! TS lets apps add custom message kinds via declaration merging on
//! `CustomAgentMessages`. Rust has no declaration merging, so we model the same
//! openness as a four-arm enum: the three base [`Message`] arms plus
//! [`CustomMessage`]. Apps extend by populating `Custom`, not by adding arms.
//!
//! Serialization mirrors the wire shape: `#[serde(tag="kind")]` with
//! `{kind:"user"|"assistant"|"toolResult"|"custom", ...}`. The three base arms
//! flatten their `Message` (which itself is tagged on `role`); we serialize
//! them via a custom impl so the outer `kind` discriminator stays consistent
//! and the inner role-specific fields survive a round-trip. `Custom` carries a
//! free-form `data: Value` so each role's structured payload rides along.

use rpi_ai::types::{AssistantMessage, Content, Message, ToolResultMessage, UserMessage};
use serde::{Deserialize, Serialize};

/// `AgentMessage = Message | Custom`. The open-enum port of TS
/// `AgentMessage = Message | CustomAgentMessages[keyof CustomAgentMessages]`.
///
/// `PartialEq` is derived so `pi-harness` entry equality (and tests) can compare
/// persisted messages by value. `rpi_ai`'s base message types already implement
/// `PartialEq`; `CustomMessage` does too, so the four-arm enum derives cleanly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentMessage {
    User(UserMessage),
    Assistant(Box<AssistantMessage>),
    ToolResult(Box<ToolResultMessage>),
    Custom(CustomMessage),
}

impl AgentMessage {
    /// The logical role tag — mirrors `message.role` on the TS side. For base
    /// messages this is `"user"`/`"assistant"`/`"toolResult"`; for custom it is
    /// the custom role string. Used by the loop's role checks (e.g. "can't
    /// continue from an assistant message").
    pub fn role(&self) -> AgentMessageRole {
        match self {
            AgentMessage::User(_) => AgentMessageRole::User,
            AgentMessage::Assistant(_) => AgentMessageRole::Assistant,
            AgentMessage::ToolResult(_) => AgentMessageRole::ToolResult,
            AgentMessage::Custom(c) => AgentMessageRole::Custom(c.role.clone()),
        }
    }

    pub fn is_assistant(&self) -> bool {
        matches!(self, AgentMessage::Assistant(_))
    }

    /// If this is an assistant message, borrow it.
    pub fn as_assistant(&self) -> Option<&AssistantMessage> {
        match self {
            AgentMessage::Assistant(a) => Some(a),
            _ => None,
        }
    }

    /// If this is an assistant message, own a clone of it.
    pub fn into_assistant(self) -> Option<AssistantMessage> {
        match self {
            AgentMessage::Assistant(a) => Some(*a),
            _ => None,
        }
    }
}

/// Serialize an `AssistantMessage` in the exact shape `AgentMessage::Assistant`
/// produces (`{"kind":"assistant", …}`), without materializing an owned
/// `AgentMessage` copy of it.
///
/// [`crate::AgentEvent::MessageUpdate`] carries the streaming partial as an
/// `Arc<AssistantMessage>` so neither the loop nor its consumers deep-clone the
/// whole growing message per delta. Consumers that need the message as JSON (the
/// remote protocol, the extension bridge) use this to keep the wire/plugin shape
/// byte-identical to a real `AgentMessage` — a deep clone here would reintroduce
/// exactly the per-delta copy the `Arc` exists to avoid. The equivalence is
/// pinned by `assistant_json_matches_the_agent_message_shape`.
///
/// [`crate::AgentEvent::MessageUpdate`]: crate::events::AgentEvent::MessageUpdate
pub fn assistant_json(message: &AssistantMessage) -> serde_json::Value {
    /// Internally tagged exactly like `AgentMessage`, so serde emits the same
    /// `"kind"` discriminator first and then the message fields.
    #[derive(serde::Serialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum Tagged<'a> {
        Assistant(&'a AssistantMessage),
    }
    serde_json::to_value(Tagged::Assistant(message)).unwrap_or(serde_json::Value::Null)
}

impl From<UserMessage> for AgentMessage {
    fn from(m: UserMessage) -> Self {
        AgentMessage::User(m)
    }
}
impl From<AssistantMessage> for AgentMessage {
    fn from(m: AssistantMessage) -> Self {
        AgentMessage::Assistant(Box::new(m))
    }
}
impl From<ToolResultMessage> for AgentMessage {
    fn from(m: ToolResultMessage) -> Self {
        AgentMessage::ToolResult(Box::new(m))
    }
}
impl From<Message> for AgentMessage {
    fn from(m: Message) -> Self {
        match m {
            Message::User(u) => AgentMessage::User(u),
            Message::Assistant(a) => AgentMessage::Assistant(a),
            Message::ToolResult(t) => AgentMessage::ToolResult(t),
        }
    }
}

/// Typed view of `AgentMessage::role`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentMessageRole {
    User,
    Assistant,
    ToolResult,
    Custom(String),
}

impl AgentMessageRole {
    pub fn as_str(&self) -> &str {
        match self {
            AgentMessageRole::User => "user",
            AgentMessageRole::Assistant => "assistant",
            AgentMessageRole::ToolResult => "toolResult",
            AgentMessageRole::Custom(s) => s.as_str(),
        }
    }
}

/// A custom app message. `role` is the app-defined tag (e.g. `"bashExecution"`,
/// `"branchSummary"`); `content` is provider-content the renderer emits;
/// `data` carries the role's structured payload; `timestamp` is ms-since-epoch.
///
/// `pi-harness::messages` provides typed constructors per known role;
/// `pi-agent` stays role-agnostic and only owns this shell.
///
/// `PartialEq` is derived so `pi-harness` entry equality (and tests) can compare
/// persisted custom messages by value. `serde_json::Value` + `Content` both
/// implement `PartialEq`, so the derive is sound.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    pub role: String,
    pub content: Vec<Content>,
    pub data: serde_json::Value,
    pub timestamp: i64,
}

impl CustomMessage {
    pub fn new(
        role: impl Into<String>,
        content: Vec<Content>,
        data: serde_json::Value,
        timestamp: i64,
    ) -> Self {
        Self {
            role: role.into(),
            content,
            data,
            timestamp,
        }
    }
}

#[cfg(test)]
mod assistant_json_tests {
    use super::*;
    use rpi_ai::types::{Api, AssistantMessage};

    #[test]
    fn assistant_json_matches_the_agent_message_shape() {
        // The whole point of `assistant_json` is that consumers can keep
        // serializing a `MessageUpdate` partial the way they always have, so it
        // must be indistinguishable from the real `AgentMessage` encoding.
        let mut assistant =
            AssistantMessage::empty(Api::OpenaiCompletions, "gateway", "gpt-test", 7);
        assistant.error_message = Some("boom".into());
        let as_agent = AgentMessage::Assistant(Box::new(assistant.clone()));
        assert_eq!(
            assistant_json(&assistant),
            serde_json::to_value(&as_agent).expect("AgentMessage serializes"),
        );
    }
}
