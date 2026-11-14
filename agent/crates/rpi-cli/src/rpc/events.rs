//! Event projection shared by the P0 native host and its clients.
use rpi_agent::events::AgentEvent;

/// Reuse the existing wire-safe event projection while the P0 host is moved
/// onto the dedicated `rpc` module. The projection contains no local handles
/// or provider credentials.
pub(crate) fn project(event: &AgentEvent) -> serde_json::Value {
    serde_json::to_value(crate::remote::protocol::RemoteEvent::from_agent_event(
        event,
    ))
    .unwrap_or(serde_json::Value::Null)
}

/// Encode compact deltas directly, inspecting only tool identity in snapshots.
/// Snapshot mode preserves the existing normalized projection.
pub(crate) fn encode(
    event: &AgentEvent,
    mode: crate::args::RpcMessageUpdates,
) -> serde_json::Result<Vec<u8>> {
    use rpi_ai::AssistantMessageEvent;
    use std::sync::Arc;
    let AgentEvent::MessageUpdate {
        message,
        assistant_message_event,
        message_id,
    } = event
    else {
        return serde_json::to_vec(&project(event));
    };
    let (index, delta, partial) = match assistant_message_event {
        AssistantMessageEvent::TextDelta {
            content_index,
            delta,
            partial,
        }
        | AssistantMessageEvent::ThinkingDelta {
            content_index,
            delta,
            partial,
        }
        | AssistantMessageEvent::ToolCallDelta {
            content_index,
            delta,
            partial,
        } => (content_index, delta, partial),
        _ => return serde_json::to_vec(&project(event)),
    };
    if mode == crate::args::RpcMessageUpdates::CompactDeltas {
        let mut nested = serde_json::json!({
            "type": assistant_message_event.type_tag(),
            "contentIndex": index,
            "delta": delta,
        });
        if matches!(
            assistant_message_event,
            AssistantMessageEvent::ToolCallDelta { .. }
        ) {
            if let Some(rpi_ai::Content::ToolCall(call)) = partial.content.get(*index) {
                nested["id"] = serde_json::json!(call.id);
                nested["toolName"] = serde_json::json!(call.name);
            }
        }
        let mut value = serde_json::json!({
            "type": "message_update",
            "assistantMessageEvent": nested,
            "eventType": assistant_message_event.type_tag(),
            "contentIndex": index,
            "delta": delta,
        });
        if let Some(id) = message_id {
            value["messageId"] = serde_json::json!(id);
        }
        return serde_json::to_vec(&value);
    }
    // Value normalizes Content's internally tagged markers just like the legacy
    // projection. Serialize this normalized snapshot once and reuse its bytes.
    let snapshot = serde_json::to_vec(&serde_json::to_value(&**message)?)?;
    let other = if Arc::ptr_eq(message, partial) {
        None
    } else {
        Some(serde_json::to_vec(&serde_json::to_value(&**partial)?)?)
    };
    let partial = other.as_deref().unwrap_or(&snapshot);
    let delta = serde_json::to_vec(delta)?;
    let tag = serde_json::to_vec(assistant_message_event.type_tag())?;
    let index = serde_json::to_vec(index)?;
    let mut out = Vec::with_capacity(snapshot.len() + partial.len() + delta.len() * 2 + 256);
    out.extend_from_slice(br#"{"type":"message_update","message":{"kind":"assistant","#);
    out.extend_from_slice(&snapshot[1..]);
    out.extend_from_slice(br#", "assistantMessageEvent":{"type":"#);
    out.extend_from_slice(&tag);
    out.extend_from_slice(br#", "contentIndex":"#);
    out.extend_from_slice(&index);
    out.extend_from_slice(br#", "delta":"#);
    out.extend_from_slice(&delta);
    out.extend_from_slice(br#", "partial":"#);
    out.extend_from_slice(partial);
    out.extend_from_slice(br#"},"eventType":"#);
    out.extend_from_slice(&tag);
    out.extend_from_slice(br#", "contentIndex":"#);
    out.extend_from_slice(&index);
    out.extend_from_slice(br#", "delta":"#);
    out.extend_from_slice(&delta);
    if let Some(id) = message_id {
        out.extend_from_slice(br#","messageId":"#);
        out.extend_from_slice(&serde_json::to_vec(id)?);
    }
    out.push(b'}');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::RpcMessageUpdates::{CompactDeltas, Snapshot};
    use rpi_ai::{Api, AssistantMessage, AssistantMessageEvent, Content};
    use std::sync::Arc;

    #[test]
    fn encoded_deltas_match_legacy_fields_with_shared_and_distinct_snapshots() {
        let delta = "quote\" slash\\ line\n nul\0 🐈";
        for shared in [false, true] {
            let mut message = AssistantMessage::empty(Api::Faux, "faux", "m", 42);
            message.content = vec![
                Content::text(delta),
                Content::thinking("reasoning"),
                Content::tool_call("call", "read", serde_json::json!({"large": delta})),
            ];
            let message = Arc::new(message);
            let partial = if shared {
                Arc::clone(&message)
            } else {
                let mut other = (*message).clone();
                other.content = vec![
                    Content::text("different snapshot"),
                    Content::thinking("other reasoning"),
                    Content::tool_call("other", "write", serde_json::json!({})),
                ];
                Arc::new(other)
            };
            for event in [
                AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: delta.into(),
                    partial: Arc::clone(&partial),
                },
                AssistantMessageEvent::ThinkingDelta {
                    content_index: 1,
                    delta: delta.into(),
                    partial: Arc::clone(&partial),
                },
                AssistantMessageEvent::ToolCallDelta {
                    content_index: 2,
                    delta: delta.into(),
                    partial: Arc::clone(&partial),
                },
            ] {
                let event = AgentEvent::MessageUpdate {
                    message_id: Some("entry-\"🐈".into()),
                    message: Arc::clone(&message),
                    assistant_message_event: event,
                };
                let bytes = encode(&event, Snapshot).unwrap();
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                    project(&event)
                );
                let compact: serde_json::Value =
                    serde_json::from_slice(&encode(&event, CompactDeltas).unwrap()).unwrap();
                let mut expected = project(&event);
                expected.as_object_mut().unwrap().remove("message");
                expected["assistantMessageEvent"]
                    .as_object_mut()
                    .unwrap()
                    .remove("partial");
                if let AgentEvent::MessageUpdate {
                    assistant_message_event: AssistantMessageEvent::ToolCallDelta { partial, .. },
                    ..
                } = &event
                {
                    let Content::ToolCall(call) = &partial.content[2] else {
                        panic!("tool")
                    };
                    expected["assistantMessageEvent"]["id"] = serde_json::json!(call.id);
                    expected["assistantMessageEvent"]["toolName"] = serde_json::json!(call.name);
                }
                assert_eq!(compact, expected);
                assert_eq!(compact["delta"], delta);
            }
        }
    }

    #[test]
    fn high_frequency_compact_bytes_grow_linearly_not_quadratically() {
        fn bytes(count: usize, mode: crate::args::RpcMessageUpdates) -> usize {
            (1..=count)
                .map(|size| {
                    let mut message = AssistantMessage::empty(Api::Faux, "faux", "m", 42);
                    message.content = vec![Content::text("x".repeat(size))];
                    let message = Arc::new(message);
                    [0, 1, 2]
                        .into_iter()
                        .map(|kind| {
                            let delta = match kind {
                                0 => AssistantMessageEvent::TextDelta {
                                    content_index: 0,
                                    delta: "x".into(),
                                    partial: message.clone(),
                                },
                                1 => AssistantMessageEvent::ThinkingDelta {
                                    content_index: 0,
                                    delta: "x".into(),
                                    partial: message.clone(),
                                },
                                _ => AssistantMessageEvent::ToolCallDelta {
                                    content_index: 0,
                                    delta: "x".into(),
                                    partial: message.clone(),
                                },
                            };
                            encode(
                                &AgentEvent::MessageUpdate {
                                    message_id: Some("entry-\"🐈".into()),
                                    message: message.clone(),
                                    assistant_message_event: delta,
                                },
                                mode,
                            )
                            .unwrap()
                            .len()
                        })
                        .sum::<usize>()
                })
                .sum()
        }
        let compact = bytes(1024, CompactDeltas);
        assert_eq!(bytes(2048, CompactDeltas), compact * 2);
        let snapshot = bytes(1024, Snapshot);
        let larger_snapshot = bytes(2048, Snapshot);
        assert!(
            larger_snapshot > snapshot * 3,
            "snapshot payload should approach quadratic growth"
        );
        assert!(larger_snapshot < snapshot * 4);
        assert!(snapshot > compact * 5);
    }

    #[test]
    fn structural_updates_and_terminal_metadata_keep_legacy_projection() {
        use rpi_ai::{DoneReason, ErrorReason, StopReason};
        for reason in [StopReason::Stop, StopReason::Error, StopReason::Aborted] {
            let mut message = AssistantMessage::empty(Api::Faux, "faux", "model", 42);
            message.content = vec![Content::text("final 🐈"), Content::thinking("thought")];
            message.stop_reason = reason;
            message.error_message = Some("diagnostic \"\\\n".into());
            message.usage.input = 123;
            message.usage.output = 45;
            message.usage.total_tokens = 168;
            message.usage.cost.output = 0.125;
            let message = Arc::new(message);
            let mut events = vec![
                AssistantMessageEvent::Start {
                    partial: message.clone(),
                },
                AssistantMessageEvent::TextStart {
                    content_index: 0,
                    partial: message.clone(),
                },
                AssistantMessageEvent::TextEnd {
                    content_index: 0,
                    content: "final 🐈".into(),
                    partial: message.clone(),
                },
                AssistantMessageEvent::ThinkingStart {
                    content_index: 1,
                    partial: message.clone(),
                },
                AssistantMessageEvent::ThinkingEnd {
                    content_index: 1,
                    content: "thought".into(),
                    partial: message.clone(),
                },
                AssistantMessageEvent::ToolCallStart {
                    content_index: 2,
                    partial: message.clone(),
                },
                AssistantMessageEvent::ToolCallEnd {
                    content_index: 2,
                    tool_call: rpi_ai::ToolCall {
                        kind: Default::default(),
                        namespace: None,
                        id: "call".into(),
                        name: "tool".into(),
                        arguments: serde_json::json!({"x": 1}),
                        thought_signature: None,
                        recorded: None,
                    },
                    partial: message.clone(),
                },
            ];
            events.push(match reason {
                StopReason::Stop => AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: (*message).clone(),
                },
                StopReason::Aborted => AssistantMessageEvent::Error {
                    reason: ErrorReason::Aborted,
                    error: (*message).clone(),
                },
                _ => AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error: (*message).clone(),
                },
            });
            let final_message: rpi_agent::message::AgentMessage = (*message).clone().into();
            let mut events: Vec<_> = events
                .into_iter()
                .map(|assistant_message_event| AgentEvent::MessageUpdate {
                    message_id: Some("entry-\"🐈".into()),
                    message: message.clone(),
                    assistant_message_event,
                })
                .collect();
            events.extend([
                AgentEvent::MessageStart {
                    message_id: Some("entry-1".into()),
                    message: final_message.clone(),
                },
                AgentEvent::MessageEnd {
                    message_id: Some("entry-1".into()),
                    message: final_message.clone(),
                },
                AgentEvent::TurnEnd {
                    message: final_message.clone(),
                    tool_results: vec![],
                    pending_messages: 0,
                },
                AgentEvent::AgentEnd {
                    messages: vec![final_message],
                },
                AgentEvent::RetryScheduled {
                    attempt: 1,
                    max_retries: 3,
                    delay_ms: 100,
                    error: "retry".into(),
                },
            ]);
            for event in events {
                for mode in [CompactDeltas, Snapshot] {
                    let actual: serde_json::Value =
                        serde_json::from_slice(&encode(&event, mode).unwrap()).unwrap();
                    assert_eq!(actual, project(&event));
                }
            }
        }
    }

    #[test]
    fn structural_events_keep_the_existing_projection() {
        for event in [
            AgentEvent::AgentStart,
            AgentEvent::TurnStart,
            AgentEvent::AgentEnd { messages: vec![] },
            AgentEvent::MessageEnd {
                message_id: Some("entry-1".into()),
                message: rpi_ai::UserMessage::new("hello", 0).into(),
            },
        ] {
            for mode in [CompactDeltas, Snapshot] {
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&encode(&event, mode).unwrap())
                        .unwrap(),
                    project(&event)
                );
            }
        }
    }
}
