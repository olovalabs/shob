//! UI-only reconstruction of compact assistant updates. This is not a lossless
//! replacement for provider messages: snapshots retain authority over metadata.
use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::protocol::RemoteEvent;

#[derive(Debug, Clone, Default)]
pub(crate) struct MessageAccumulator {
    message: Value,
    blocks: BTreeMap<u64, Value>,
    tool_json: BTreeMap<u64, String>,
}

impl MessageAccumulator {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn snapshot(&mut self, message: &Value) {
        self.message = message.clone();
        self.blocks = message
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, block)| (index as u64, block.clone()))
            .collect();
    }

    fn projected_message(&self) -> Value {
        let mut message = if self.message.is_object() {
            self.message.clone()
        } else {
            json!({"role": "assistant"})
        };
        message["content"] = Value::Array(self.blocks.values().cloned().collect());
        message
    }

    /// Normalize before UI projection; never append a delta to its own snapshot.
    pub(crate) fn apply(&mut self, event: &RemoteEvent) -> RemoteEvent {
        match event {
            RemoteEvent::AgentStart
            | RemoteEvent::AgentEnd { .. }
            | RemoteEvent::RetryScheduled { .. }
            | RemoteEvent::TurnStart
            | RemoteEvent::TurnEnd { .. } => self.reset(),
            RemoteEvent::MessageStart { message, .. } => {
                self.reset();
                self.snapshot(message);
            }
            RemoteEvent::MessageEnd { .. } => self.reset(),
            RemoteEvent::MessageUpdate {
                message,
                assistant_message_event: nested,
                event_type,
                content_index,
                delta,
                ..
            } => {
                let kind = if event_type.is_empty() {
                    nested.get("type").and_then(Value::as_str).unwrap_or("")
                } else {
                    event_type.as_str()
                };
                let index =
                    content_index.or_else(|| nested.get("contentIndex").and_then(Value::as_u64));
                let delta = delta
                    .as_deref()
                    .or_else(|| nested.get("delta").and_then(Value::as_str));
                // Top-level messages can intentionally differ from nested partials.
                let snapshot = if message.is_object() {
                    Some(message)
                } else {
                    ["partial", "message", "error"]
                        .iter()
                        .find_map(|key| nested.get(*key).filter(|value| value.is_object()))
                };
                if kind == "start" {
                    self.reset();
                }
                if let Some(index) = index {
                    if matches!(kind, "toolcall_start" | "tool_call_start") {
                        self.tool_json.remove(&index);
                    }
                    // Keep the raw argument stream even on snapshot events, so
                    // switching between snapshot and compact updates is safe.
                    if matches!(kind, "toolcall_delta" | "tool_call_delta") {
                        if let Some(delta) = delta {
                            self.tool_json.entry(index).or_default().push_str(delta);
                        }
                    }
                }
                if let Some(snapshot) = snapshot {
                    self.snapshot(snapshot);
                } else if let Some(index) = index {
                    match kind {
                        "text_delta" | "thinking_delta" => {
                            let (ty, field) = if kind == "text_delta" {
                                ("text", "text")
                            } else {
                                ("thinking", "thinking")
                            };
                            let block = self
                                .blocks
                                .entry(index)
                                .or_insert_with(|| json!({"type": ty, (field): ""}));
                            let mut text = block
                                .get(field)
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned();
                            text.push_str(delta.unwrap_or(""));
                            block[field] = Value::String(text);
                        }
                        "toolcall_delta" | "tool_call_delta" => {
                            // Use only provider-supplied identity metadata;
                            // never invent an identity from the content index.
                            let block = self
                                .blocks
                                .entry(index)
                                .or_insert_with(|| json!({"type": "toolCall"}));
                            for (source, target) in [("id", "id"), ("toolName", "name")] {
                                if let Some(value) = nested.get(source).and_then(Value::as_str) {
                                    block[target] = Value::String(value.to_owned());
                                }
                            }
                            block["arguments"] = rpi_ai::json_parse::parse_streaming_json(
                                self.tool_json.get(&index).map(String::as_str),
                            );
                        }
                        "text_end" | "thinking_end" => {
                            let (ty, field) = if kind == "text_end" {
                                ("text", "text")
                            } else {
                                ("thinking", "thinking")
                            };
                            if let Some(content) = nested.get("content").and_then(Value::as_str) {
                                self.blocks
                                    .insert(index, json!({"type": ty, (field): content}));
                            }
                        }
                        _ => {}
                    }
                }
                // The explicit final payload is more precise than a streaming
                // partial (including the tool's eventual id/name/arguments).
                if matches!(kind, "toolcall_end" | "tool_call_end") {
                    if let (Some(index), Some(tool)) = (
                        index,
                        nested.get("toolCall").filter(|tool| tool.is_object()),
                    ) {
                        let mut tool = tool.clone();
                        tool["type"] = json!("toolCall");
                        self.blocks.insert(index, tool);
                        self.tool_json.remove(&index);
                    }
                }
                let mut normalized = event.clone();
                if let RemoteEvent::MessageUpdate { message, .. } = &mut normalized {
                    *message = self.projected_message();
                }
                if matches!(kind, "done" | "error") {
                    self.reset();
                }
                return normalized;
            }
            _ => {}
        }
        event.clone()
    }
}
