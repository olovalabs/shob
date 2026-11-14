use crate::{
    AssistantMessage, AssistantMessageEvent as Event, Content, Model, StopReason, TextContent,
    ThinkingContent, ToolCall,
};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Default)]
pub(super) struct State {
    indices: BTreeMap<u64, usize>,
    arguments: BTreeMap<u64, String>,
    pub stopped: bool,
    started: bool,
}
impl State {
    pub fn apply(
        &mut self,
        kind: &str,
        value: &Value,
        out: &mut AssistantMessage,
        model: &Model,
    ) -> Result<Vec<Event>, String> {
        let mut events = Vec::new();
        if kind == "messageStart" {
            if value["role"] != "assistant" {
                return Err("Bedrock messageStart role is not assistant".into());
            }
            if !self.started {
                self.started = true;
                events.push(Event::Start {
                    partial: Arc::new(out.clone()),
                });
            }
            return Ok(events);
        }
        if matches!(kind, "contentBlockStart" | "contentBlockDelta") && !self.started {
            self.started = true;
            events.push(Event::Start {
                partial: Arc::new(out.clone()),
            });
        }
        match kind {
            "contentBlockStart" => {
                if let Some(tool) = value["start"].get("toolUse") {
                    let wire = index(value)?;
                    if self.indices.contains_key(&wire) {
                        return Err("Duplicate Bedrock block start".into());
                    }
                    let content_index = out.content.len();
                    self.indices.insert(wire, content_index);
                    self.arguments.insert(wire, String::new());
                    out.content.push(Content::ToolCall(ToolCall {
                        kind: Default::default(),
                        id: tool["toolUseId"]
                            .as_str()
                            .ok_or("Missing Bedrock toolUseId")?
                            .into(),
                        name: tool["name"]
                            .as_str()
                            .ok_or("Missing Bedrock tool name")?
                            .into(),
                        arguments: serde_json::json!({}),
                        thought_signature: None,
                        namespace: None,
                        recorded: None,
                    }));
                    events.push(Event::ToolCallStart {
                        content_index,
                        partial: Arc::new(out.clone()),
                    });
                }
            }
            "contentBlockDelta" => {
                let wire = index(value)?;
                let delta = &value["delta"];
                let content_index = if let Some(index) = self.indices.get(&wire) {
                    *index
                } else {
                    let content_index = out.content.len();
                    let block = if delta.get("text").is_some() {
                        Content::Text(TextContent {
                            kind: Default::default(),
                            text: String::new(),
                            text_signature: None,
                        })
                    } else if delta.get("reasoningContent").is_some() {
                        Content::Thinking(ThinkingContent {
                            kind: Default::default(),
                            thinking: String::new(),
                            thinking_signature: None,
                            redacted: false,
                        })
                    } else {
                        return Err("Bedrock delta references unknown tool block".into());
                    };
                    let text = matches!(&block, Content::Text(_));
                    out.content.push(block);
                    self.indices.insert(wire, content_index);
                    let partial = Arc::new(out.clone());
                    events.push(if text {
                        Event::TextStart {
                            content_index,
                            partial,
                        }
                    } else {
                        Event::ThinkingStart {
                            content_index,
                            partial,
                        }
                    });
                    content_index
                };
                let event = match &mut out.content[content_index] {
                    Content::Text(text) => {
                        let d = delta["text"].as_str().ok_or("Invalid Bedrock text delta")?;
                        text.text.push_str(d);
                        Some((0, d.to_owned()))
                    }
                    Content::ToolCall(tool) => {
                        let d = delta["toolUse"]["input"]
                            .as_str()
                            .ok_or("Invalid Bedrock tool input delta")?;
                        let raw = self
                            .arguments
                            .get_mut(&wire)
                            .ok_or("Tool block already closed")?;
                        raw.push_str(d);
                        tool.arguments =
                            crate::providers::anthropic::json_parse::parse_streaming_json(Some(
                                raw.as_str(),
                            ));
                        Some((1, d.to_owned()))
                    }
                    Content::Thinking(thinking) => {
                        let reasoning = &delta["reasoningContent"];
                        if let Some(sig) = reasoning["signature"].as_str() {
                            thinking
                                .thinking_signature
                                .get_or_insert_with(String::new)
                                .push_str(sig);
                        }
                        if let Some(redacted) = reasoning["redactedContent"].as_str() {
                            thinking.redacted = true;
                            thinking
                                .thinking_signature
                                .get_or_insert_with(String::new)
                                .push_str(redacted);
                        }
                        reasoning["text"].as_str().map(|d| {
                            thinking.thinking.push_str(d);
                            (2, d.to_owned())
                        })
                    }
                    _ => return Err("Invalid Bedrock stream block".into()),
                };
                if let Some((kind, delta)) = event {
                    let partial = Arc::new(out.clone());
                    events.push(match kind {
                        0 => Event::TextDelta {
                            content_index,
                            delta,
                            partial,
                        },
                        1 => Event::ToolCallDelta {
                            content_index,
                            delta,
                            partial,
                        },
                        _ => Event::ThinkingDelta {
                            content_index,
                            delta,
                            partial,
                        },
                    });
                }
            }
            "contentBlockStop" => {
                let wire = index(value)?;
                if let Some(content_index) = self.indices.remove(&wire) {
                    if let Some(raw) = self.arguments.remove(&wire) {
                        if let Content::ToolCall(tool) = &mut out.content[content_index] {
                            tool.arguments = if raw.trim().is_empty() {
                                serde_json::json!({})
                            } else {
                                serde_json::from_str(&raw)
                                    .map_err(|e| format!("Invalid Bedrock tool JSON: {e}"))?
                            };
                        }
                    }
                    let partial = Arc::new(out.clone());
                    events.push(match &out.content[content_index] {
                        Content::Text(t) => Event::TextEnd {
                            content_index,
                            content: t.text.clone(),
                            partial,
                        },
                        Content::Thinking(t) => Event::ThinkingEnd {
                            content_index,
                            content: t.thinking.clone(),
                            partial,
                        },
                        Content::ToolCall(t) => Event::ToolCallEnd {
                            content_index,
                            tool_call: t.clone(),
                            partial,
                        },
                        _ => return Err("Invalid Bedrock block stop".into()),
                    });
                }
            }
            "messageStop" => {
                let reason = value["stopReason"].as_str().unwrap_or("unknown");
                out.raw_stop_reason = Some(reason.into());
                self.stopped = true;
                out.stop_reason = match reason {
                    "end_turn" | "stop_sequence" => StopReason::Stop,
                    "max_tokens" | "model_context_window_exceeded" => StopReason::Length,
                    "tool_use" => StopReason::ToolUse,
                    _ => return Err(format!("Bedrock generation failed: {reason}")),
                };
                out.end_turn = Some(out.stop_reason != StopReason::ToolUse);
            }
            "metadata" => {
                if let Some(usage) = value.get("usage") {
                    out.usage.input = usage["inputTokens"].as_i64().unwrap_or(0);
                    out.usage.output = usage["outputTokens"].as_i64().unwrap_or(0);
                    out.usage.cache_read = usage["cacheReadInputTokens"].as_i64().unwrap_or(0);
                    out.usage.cache_write = usage["cacheWriteInputTokens"].as_i64().unwrap_or(0);
                    out.usage.total_tokens = usage["totalTokens"]
                        .as_i64()
                        .unwrap_or(out.usage.input + out.usage.output);
                    out.usage.cost =
                        crate::providers::anthropic::cost::calculate_cost(&model.cost, &out.usage);
                }
            }
            _ => {}
        }
        Ok(events)
    }
    pub fn finish(&self) -> Result<(), String> {
        if !self.stopped {
            Err("Truncated Bedrock stream: missing messageStop".into())
        } else if !self.indices.is_empty() {
            Err("Truncated Bedrock stream: unclosed content block".into())
        } else {
            Ok(())
        }
    }
}
fn index(value: &Value) -> Result<u64, String> {
    value["contentBlockIndex"]
        .as_u64()
        .ok_or_else(|| "Missing Bedrock contentBlockIndex".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn streams_text_tools_reasoning_and_usage() {
        let model = Model::new(
            "claude",
            "Claude",
            crate::Api::BedrockConverseStream,
            "amazon-bedrock",
            "https://example.com",
        );
        let mut out = AssistantMessage::empty(model.api.clone(), &model.provider, &model.id, 0);
        let mut state = State::default();
        state
            .apply(
                "messageStart",
                &json!({"role":"assistant"}),
                &mut out,
                &model,
            )
            .unwrap();
        let events = state
            .apply(
                "contentBlockDelta",
                &json!({"contentBlockIndex":0,"delta":{"text":"hello"}}),
                &mut out,
                &model,
            )
            .unwrap();
        assert!(matches!(
            events.as_slice(),
            [Event::TextStart { .. }, Event::TextDelta { .. }]
        ));
        state
            .apply(
                "contentBlockStop",
                &json!({"contentBlockIndex":0}),
                &mut out,
                &model,
            )
            .unwrap();
        state.apply("contentBlockStart", &json!({"contentBlockIndex":1,"start":{"toolUse":{"toolUseId":"t1","name":"read"}}}), &mut out, &model).unwrap();
        for part in ["{\"path\":", "\"a\"}"] {
            state
                .apply(
                    "contentBlockDelta",
                    &json!({"contentBlockIndex":1,"delta":{"toolUse":{"input":part}}}),
                    &mut out,
                    &model,
                )
                .unwrap();
        }
        let events = state
            .apply(
                "contentBlockStop",
                &json!({"contentBlockIndex":1}),
                &mut out,
                &model,
            )
            .unwrap();
        assert!(
            matches!(&events[0], Event::ToolCallEnd { tool_call, .. } if tool_call.arguments == json!({"path":"a"}))
        );
        state.apply("contentBlockDelta", &json!({"contentBlockIndex":2,"delta":{"reasoningContent":{"text":"think","signature":"sig"}}}), &mut out, &model).unwrap();
        state
            .apply(
                "contentBlockStop",
                &json!({"contentBlockIndex":2}),
                &mut out,
                &model,
            )
            .unwrap();
        state
            .apply(
                "messageStop",
                &json!({"stopReason":"tool_use"}),
                &mut out,
                &model,
            )
            .unwrap();
        state
            .apply(
                "metadata",
                &json!({"usage":{"inputTokens":10,"outputTokens":5,"cacheReadInputTokens":7}}),
                &mut out,
                &model,
            )
            .unwrap();
        state.finish().unwrap();
        assert_eq!(out.stop_reason, StopReason::ToolUse);
        assert_eq!(out.usage.cache_read, 7);
        assert!(
            matches!(&out.content[2], Content::Thinking(t) if t.thinking_signature.as_deref() == Some("sig"))
        );
    }
    #[test]
    fn rejects_truncation_and_guardrail_stops() {
        assert!(State::default().finish().is_err());
        let model = Model::new(
            "a",
            "a",
            crate::Api::BedrockConverseStream,
            "amazon-bedrock",
            "https://example.com",
        );
        let mut out = AssistantMessage::empty(model.api.clone(), &model.provider, &model.id, 0);
        assert!(State::default()
            .apply(
                "messageStop",
                &json!({"stopReason":"guardrail_intervened"}),
                &mut out,
                &model
            )
            .unwrap_err()
            .contains("guardrail_intervened"));
    }
}
