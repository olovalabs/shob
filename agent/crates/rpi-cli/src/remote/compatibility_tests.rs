//! Fixture-based tests for the exact stateful adapter used by RemoteDriver.
//! Includes real server encoding to cover compact wire metadata.
use super::*;

fn decode(value: Value) -> RemoteEvent {
    serde_json::from_value(value).unwrap()
}

fn message(content: Value) -> Value {
    json!({"kind":"assistant", "content":content, "stopReason":"stop", "customMetadata":{"keep":true}})
}

fn update(kind: &str, index: u64, delta: Option<&str>, content: Value) -> Value {
    let snapshot = message(content);
    let mut event = json!({
        "type":"message_update", "eventType":kind, "contentIndex":index,
        "message":snapshot,
        "assistantMessageEvent":{"type":kind,"contentIndex":index,"partial":snapshot}
    });
    if let Some(delta) = delta {
        event["delta"] = json!(delta);
        event["assistantMessageEvent"]["delta"] = json!(delta);
    }
    event
}

fn compact(mut event: Value) -> Value {
    if matches!(
        event["eventType"].as_str(),
        Some("text_delta" | "thinking_delta" | "toolcall_delta")
    ) {
        if event["eventType"] == "toolcall_delta" {
            if let Some(index) = event["contentIndex"].as_u64() {
                let block =
                    event["assistantMessageEvent"]["partial"]["content"][index as usize].clone();
                if block["type"] == "toolCall" {
                    event["assistantMessageEvent"]["id"] = block["id"].clone();
                    event["assistantMessageEvent"]["toolName"] = block["name"].clone();
                }
            }
        }
        event.as_object_mut().unwrap().remove("message");
        event["assistantMessageEvent"]
            .as_object_mut()
            .unwrap()
            .remove("partial");
    }
    event
}

// DriverEvent/UiEvent intentionally have no PartialEq. Debug captures all their
// projected fields (including block order and tool identities/arguments).
fn assert_stream_equivalent(stream: &[Value]) -> Vec<DriverEvent> {
    let mut snapshots = RemoteEventMapper::default();
    let mut compacted = RemoteEventMapper::default();
    let mut full_session = RemoteSession::new();
    let mut compact_session = RemoteSession::new();
    let mut events = Vec::new();
    for (index, line) in stream.iter().enumerate() {
        let full = decode(line.clone());
        let small = decode(compact(line.clone()));
        let expected = snapshots.map(&full);
        let actual = compacted.map(&small);
        assert_eq!(
            format!("{actual:?}"),
            format!("{expected:?}"),
            "event {index}: {line}"
        );
        full_session.apply_event(&full);
        compact_session.apply_event(&small);
        assert_eq!(
            full_session.transcript.items, compact_session.transcript.items,
            "session at {index}"
        );
        assert_eq!(full_session.streaming, compact_session.streaming);
        if let Some(actual) = actual {
            events.push(actual);
        }
    }
    events
}

#[tokio::test]
async fn real_completions_provider_keeps_compact_channel_order() {
    assert_real_provider_channel_order(rpi_ai::Api::OpenaiCompletions).await;
}

#[tokio::test]
async fn real_responses_provider_keeps_compact_channel_order() {
    assert_real_provider_channel_order(rpi_ai::Api::OpenaiResponses).await;
}

/// Exercise the public HTTP provider, not a synthetic provider-event fixture.
/// Hold the terminal SSE frame back until TextStart is observed so a final
/// authoritative snapshot cannot hide an intermediate ordering regression.
async fn assert_real_provider_channel_order(api: rpi_ai::Api) {
    use crate::args::RpcMessageUpdates::{CompactDeltas, Snapshot};
    use rpi_agent::events::AgentEvent;
    use rpi_ai::{
        providers::{
            openai_completions::OpenAiCompletionsProvider,
            openai_responses::OpenAiResponsesProvider,
        },
        Api, AssistantMessageEvent, Context, Model, Provider, SimpleStreamOptions, StopReason,
    };
    use std::{sync::Arc, time::Duration};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    tokio::time::timeout(Duration::from_secs(15), async {
        let sse = |value: Value| format!("data: {value}\n\n");
        let (prefix, suffix) = if api == Api::OpenaiCompletions {
            let delta = |value| sse(json!({"choices":[{"delta":value}]}));
            (
                [
                    delta(json!({"reasoning_content":"考"})),
                    delta(json!({"reasoning_content":"える"})),
                    delta(json!({"content":"Hé"})),
                ].concat(),
                [
                    delta(json!({"content":"🌍"})),
                    delta(json!({"reasoning_content":"🧠"})),
                    delta(json!({"content":"!"})),
                    delta(json!({"tool_calls":[{"index":0,"id":"call-real","type":"function","function":{"name":"read","arguments":"{\"path\":\""}}]})),
                    delta(json!({"reasoning_content":"終"})),
                    delta(json!({"tool_calls":[{"index":0,"function":{"arguments":"é\"}"}}]})),
                    delta(json!({"content":" Done"})),
                    sse(json!({"choices":[{"delta":{},"finish_reason":"stop"}]})),
                    "data: [DONE]\n\n".into(),
                ].concat(),
            )
        } else {
            let delta = |kind: &str, index, text: &str| {
                sse(json!({"type":kind,"output_index":index,"delta":text}))
            };
            (
                [
                    sse(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"r"}})),
                    delta("response.reasoning_text.delta", 0, "考"),
                    delta("response.reasoning_text.delta", 0, "える"),
                    sse(json!({"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"m","role":"assistant","content":[]}})),
                    delta("response.output_text.delta", 1, "Hé"),
                ].concat(),
                [
                    delta("response.output_text.delta", 1, "🌍"),
                    delta("response.reasoning_text.delta", 0, "🧠"),
                    delta("response.output_text.delta", 1, "!"),
                    sse(json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc","call_id":"call-real","name":"read","arguments":""}})),
                    delta("response.function_call_arguments.delta", 2, "{\"path\":\""),
                    delta("response.reasoning_text.delta", 0, "終"),
                    delta("response.function_call_arguments.delta", 2, "é\"}"),
                    delta("response.output_text.delta", 1, " Done"),
                    sse(json!({"type":"response.completed","response":{"status":"completed"}})),
                ].concat(),
            )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (release, wait) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut bytes = [0; 4096];
                let n = socket.read(&mut bytes).await.unwrap();
                assert_ne!(n, 0);
                request.extend_from_slice(&bytes[..n]);
                if let Some(end) = request.windows(4).position(|b| b == b"\r\n\r\n") {
                    let length: usize = String::from_utf8_lossy(&request[..end])
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                prefix.len() + suffix.len()
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(prefix.as_bytes()).await.unwrap();
            wait.await.unwrap();
            socket.write_all(suffix.as_bytes()).await.unwrap();
        });
        let model = Model::new("fixture", "Fixture", api.clone(), "fixture", format!("http://{address}"));
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let provider: Box<dyn Provider> = if api == Api::OpenaiCompletions {
            Box::new(OpenAiCompletionsProvider::with_models_without_env_api_key(
                "fixture", Some("offline-fixture-key".into()), client, vec![model.clone()],
            ))
        } else {
            Box::new(OpenAiResponsesProvider::with_models_without_env_api_key(
                "fixture", Some("offline-fixture-key".into()), client, vec![model.clone()],
            ))
        };
        let mut stream = provider.stream_simple(&model, &Context::new(vec![]), &SimpleStreamOptions::default()).await;
        let mut full_mapper = RemoteEventMapper::default();
        let mut compact_mapper = RemoteEventMapper::default();
        let mut release = Some(release);
        let mut thinking = String::new();
        let mut saw_done = false;
        let mut tags = Vec::new();
        while let Some(nested) = stream.next().await {
            tags.push(nested.type_tag());
            if let AssistantMessageEvent::ThinkingDelta { delta, .. } = &nested {
                thinking.push_str(delta);
            }
            if matches!(nested, AssistantMessageEvent::TextStart { .. }) {
                // This assertion runs while the server is still withholding
                // every remaining update, including the terminal frame.
                assert_eq!(thinking, "考える", "thinking must precede TextStart: {tags:?}");
                release.take().unwrap().send(()).unwrap();
            }
            assert!(!matches!(nested, AssistantMessageEvent::Error { .. }), "{nested:?}");
            if let AssistantMessageEvent::Done { message, .. } = &nested {
                saw_done = true;
                assert_eq!(message.stop_reason, if api == Api::OpenaiResponses { StopReason::ToolUse } else { StopReason::Stop });
                assert!(matches!(&message.content[0], rpi_ai::Content::Thinking(block) if block.thinking == "考える🧠終"));
                assert!(matches!(&message.content[1], rpi_ai::Content::Text(block) if block.text == "Hé🌍! Done"));
            }
            let event = AgentEvent::MessageUpdate { message_id: None,
                message: Arc::new(nested.partial().clone()),
                assistant_message_event: nested,
            };
            let encode = |mode| decode(serde_json::from_slice(&crate::rpc::events::encode(&event, mode).unwrap()).unwrap());
            let full = full_mapper.map(&encode(Snapshot));
            let small = compact_mapper.map(&encode(CompactDeltas));
            // Driver events contain the rendered blocks/tool calls, not
            // response metadata, which is allowed to arrive at different times.
            assert_eq!(format!("{small:?}"), format!("{full:?}"), "{api:?}: {tags:?}");
        }
        assert!(saw_done);
        assert!(release.is_none());
        server.await.unwrap();
    }).await.expect("offline provider stream timed out");
}

#[test]
fn real_encoder_preserves_first_and_late_tool_identity_after_empty_start() {
    use crate::args::RpcMessageUpdates::{CompactDeltas, Snapshot};
    use rpi_agent::events::AgentEvent;
    use rpi_ai::{Api, AssistantMessage, AssistantMessageEvent, Content};
    use std::sync::Arc;

    // Completions emits the start before processing the first identity
    // fragments. Both first-fragment and later identity updates must reach UI.
    for identity_on_first in [false, true] {
        let mut full_mapper = RemoteEventMapper::default();
        let mut compact_mapper = RemoteEventMapper::default();
        for step in 0..4 {
            let known = step >= if identity_on_first { 1 } else { 2 };
            let (id, name) = if known {
                ("call-real", "read")
            } else {
                ("", "")
            };
            let mut partial = AssistantMessage::empty(Api::Faux, "faux", "m", 1);
            partial.content = vec![Content::tool_call(
                id,
                name,
                match step {
                    0 => json!({}),
                    1 => json!({"path":""}),
                    _ => json!({"path":"prefix"}),
                },
            )];
            let partial = Arc::new(partial);
            let nested = if step == 0 {
                AssistantMessageEvent::ToolCallStart {
                    content_index: 0,
                    partial: partial.clone(),
                }
            } else {
                AssistantMessageEvent::ToolCallDelta {
                    content_index: 0,
                    delta: ["", "{\"path\":\"", "prefix", "\"}"][step].into(),
                    partial: partial.clone(),
                }
            };
            let event = AgentEvent::MessageUpdate {
                message_id: None,
                message: partial,
                assistant_message_event: nested,
            };
            let encode = |mode| {
                decode(
                    serde_json::from_slice(&crate::rpc::events::encode(&event, mode).unwrap())
                        .unwrap(),
                )
            };
            let full = full_mapper.map(&encode(Snapshot));
            let small = compact_mapper.map(&encode(CompactDeltas));
            assert_eq!(format!("{small:?}"), format!("{full:?}"));
            let Some(DriverEvent::Ui(UiEvent::AssistantUpdate { tool_calls, .. })) = small else {
                panic!("missing update")
            };
            assert_eq!(tool_calls.len(), usize::from(known));
            if known {
                assert_eq!(tool_calls[0].id, "call-real");
                assert_eq!(tool_calls[0].name, "read");
            }
        }
    }
}

#[test]
fn compact_driver_matches_unicode_interleaved_tools_and_final_corrections() {
    let mut content = json!([
        {"type":"text","text":"Initial: "},
        {"type":"thinking","thinking":"考"},
        {"type":"toolCall","id":"","name":"","arguments":{}},
        {"type":"toolCall","id":"known","name":"read","arguments":{}}
    ]);
    let mut stream = vec![
        json!({"type":"agent_start"}),
        json!({"type":"message_start", "message":message(content.clone())}),
        update("toolcall_start", 2, None, content.clone()),
        update("toolcall_start", 3, None, content.clone()),
    ];
    content[0]["text"] = json!("Initial: Hé🌍");
    stream.push(update("text_delta", 0, Some("Hé🌍"), content.clone()));
    content[1]["thinking"] = json!("考える🧠");
    stream.push(update("thinking_delta", 1, Some("える🧠"), content.clone()));
    // Interleave two independent JSON streams. Unknown identities must not
    // become a synthetic index-based call, even after valid JSON arrives.
    content[2]["arguments"] = json!({"path":""});
    stream.push(update(
        "toolcall_delta",
        2,
        Some("{\"path\":\""),
        content.clone(),
    ));
    content[3]["arguments"] = json!({"n":1});
    stream.push(update(
        "toolcall_delta",
        3,
        Some("{\"n\":1"),
        content.clone(),
    ));
    content[2]["arguments"] = json!({"path":"目录/é"});
    stream.push(update(
        "toolcall_delta",
        2,
        Some("目录/é\"}"),
        content.clone(),
    ));
    content[3]["arguments"] = json!({"n":12});
    stream.push(update("toolcall_delta", 3, Some("2}"), content.clone()));
    // Late id/name arrives through a structural snapshot, not an invented
    // identity on a delta. The final tool payload corrects arguments as well.
    content[2] =
        json!({"type":"toolCall","id":"late","name":"write","arguments":{"path":"corrected"}});
    let mut end = update("toolcall_end", 2, None, content.clone());
    end["assistantMessageEvent"]["toolCall"] = content[2].clone();
    stream.push(end);
    content[0]["text"] = json!("Corrected final 🌍");
    content[1]["thinking"] = json!("最終🧠");
    stream.push(json!({"type":"message_update","eventType":"done", "message":message(content.clone()),
        "assistantMessageEvent":{"type":"done","reason":"stop","message":message(content.clone())}}));
    stream.push(json!({"type":"message_end", "message":message(content)}));
    stream.push(json!({"type":"tool_execution_start","toolCallId":"late","toolName":"write","args":{"path":"corrected"}}));
    stream.push(json!({"type":"tool_execution_update","toolCallId":"late","toolName":"write","args":{"path":"corrected"},"partialResult":{"content":[{"type":"text","text":"写入"}]}}));
    stream.push(json!({"type":"tool_execution_end","toolCallId":"late","toolName":"write","isError":true,"result":{"content":[{"type":"text","text":"denied"}]}}));
    stream.push(json!({"type":"agent_end","messageCount":1,"messages":[]}));
    let events = assert_stream_equivalent(&stream);
    let DriverEvent::Ui(UiEvent::AssistantUpdate { tool_calls, .. }) = &events[9] else {
        panic!("missing update")
    };
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, "known");
    assert_eq!(tool_calls[0].args, json!({"n":12}));
    let DriverEvent::Ui(UiEvent::AssistantUpdate { tool_calls, .. }) = &events[10] else {
        panic!("missing tool end")
    };
    assert_eq!(tool_calls[0].id, "late");
    assert_eq!(tool_calls[0].name, "write");
    assert_eq!(tool_calls[0].args, json!({"path":"corrected"}));
    let DriverEvent::Ui(UiEvent::AssistantEnd { blocks }) = &events[12] else {
        panic!("missing message end")
    };
    assert_eq!(
        blocks,
        &vec![
            AssistantBlock::Text("Corrected final 🌍".into()),
            AssistantBlock::Thinking("最終🧠".into())
        ]
    );
}

#[test]
fn top_level_snapshot_wins_over_partial_and_delta_is_not_applied_twice() {
    let mut mapper = RemoteEventMapper::default();
    let mut line = update(
        "text_delta",
        0,
        Some("ignored"),
        json!([{"type":"text","text":"authority"}]),
    );
    line["assistantMessageEvent"]["partial"] =
        message(json!([{"type":"text","text":"different partial"}]));
    let projected = mapper.map(&decode(line));
    assert!(
        matches!(projected, Some(DriverEvent::Ui(UiEvent::AssistantUpdate { blocks, .. }))
        if blocks == vec![AssistantBlock::Text("authority".into())])
    );
    let projected = mapper.map(&decode(
        json!({"type":"message_update","eventType":"text_delta","contentIndex":0,"delta":" 🌍"}),
    ));
    assert!(
        matches!(projected, Some(DriverEvent::Ui(UiEvent::AssistantUpdate { blocks, .. }))
        if blocks == vec![AssistantBlock::Text("authority 🌍".into())])
    );
}

#[test]
fn explicit_final_tool_payload_overrides_streaming_arguments_and_identity() {
    let mut mapper = RemoteEventMapper::default();
    let mut line = update(
        "toolcall_end",
        0,
        None,
        json!([{"type":"toolCall","id":"","name":"","arguments":{"stale":true}}]),
    );
    line["assistantMessageEvent"]["toolCall"] =
        json!({"id":"real","name":"read","arguments":{"final":true}});
    let Some(DriverEvent::Ui(UiEvent::AssistantUpdate { tool_calls, .. })) =
        mapper.map(&decode(line))
    else {
        panic!("missing tool")
    };
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, "real");
    assert_eq!(tool_calls[0].name, "read");
    assert_eq!(tool_calls[0].args, json!({"final":true}));
}

#[test]
fn errors_aborts_and_retries_do_not_leak_previous_content() {
    for reason in ["error", "aborted"] {
        let final_message = json!({"kind":"assistant","content":[{"type":"text","text":"terminal correction"}],"stopReason":reason,"errorMessage":"failed"});
        let stream = vec![
            json!({"type":"agent_start"}),
            json!({"type":"message_start","message":message(json!([]))}),
            update(
                "text_delta",
                0,
                Some("temporary"),
                json!([{"type":"text","text":"temporary"}]),
            ),
            json!({"type":"message_update","eventType":"error","message":final_message,
                "assistantMessageEvent":{"type":"error","reason":reason,"error":final_message}}),
            json!({"type":"message_end","message":final_message}),
            json!({"type":"retry_scheduled","attempt":1,"maxRetries":2,"delayMs":1,"error":reason}),
            json!({"type":"message_start","message":message(json!([]))}),
            update(
                "text_delta",
                0,
                Some("fresh"),
                json!([{"type":"text","text":"fresh"}]),
            ),
            json!({"type":"agent_end","messageCount":1,"messages":[]}),
        ];
        assert_stream_equivalent(&stream);
    }
    // Test each reset independently, including interrupted streams without an
    // ordinary message_end (cancel/disconnect/retry boundaries).
    for boundary in [
        json!({"type":"agent_start"}),
        json!({"type":"agent_end","messageCount":0,"messages":[]}),
        json!({"type":"retry_scheduled","attempt":1,"maxRetries":2,"delayMs":1,"error":"retry"}),
        json!({"type":"turn_start"}),
        json!({"type":"turn_end","message":{},"toolResultCount":0,"toolResults":[]}),
        json!({"type":"message_start","message":{"kind":"user","content":"new"}}),
        json!({"type":"message_end","message":message(json!([]))}),
    ] {
        let mut mapper = RemoteEventMapper::default();
        mapper.map(&decode(update(
            "text_delta",
            0,
            Some("old"),
            json!([{"type":"text","text":"old"}]),
        )));
        mapper.map(&decode(boundary));
        let event = mapper.map(&decode(json!({"type":"message_update","eventType":"text_delta","contentIndex":0,"delta":"new"})));
        assert!(
            matches!(event, Some(DriverEvent::Ui(UiEvent::AssistantUpdate { blocks, .. }))
            if blocks == vec![AssistantBlock::Text("new".into())])
        );
    }
}

#[test]
fn mixed_snapshot_and_compact_tool_deltas_keep_the_raw_json_stream() {
    let mut mapper = RemoteEventMapper::default();
    mapper.map(&decode(update(
        "toolcall_start",
        0,
        None,
        json!([{"type":"toolCall","id":"c","name":"read","arguments":{}}]),
    )));
    mapper.map(&decode(update(
        "toolcall_delta",
        0,
        Some("{\"path\":\""),
        json!([{"type":"toolCall","id":"c","name":"read","arguments":{"path":""}}]),
    )));
    let event = mapper.map(&decode(json!({"type":"message_update","eventType":"toolcall_delta","contentIndex":0,"delta":"é\"}"})));
    let Some(DriverEvent::Ui(UiEvent::AssistantUpdate { tool_calls, .. })) = event else {
        panic!("missing update")
    };
    assert_eq!(tool_calls[0].args, json!({"path":"é"}));
}

#[test]
fn nested_fields_and_sparse_indices_do_not_fabricate_tool_identities() {
    let mut mapper = RemoteEventMapper::default();
    let tool = mapper.map(&decode(json!({
        "type":"message_update",
        "assistantMessageEvent":{"type":"toolcall_delta","contentIndex":9000000000_u64,"delta":"{\"x\":true}"}
    })));
    assert!(
        matches!(tool, Some(DriverEvent::Ui(UiEvent::AssistantUpdate { tool_calls, .. })) if tool_calls.is_empty())
    );
    let text = mapper.map(&decode(json!({
        "type":"message_update",
        "assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"你好🌍"}
    })));
    assert!(
        matches!(text, Some(DriverEvent::Ui(UiEvent::AssistantUpdate { blocks, tool_calls }))
        if blocks == vec![AssistantBlock::Text("你好🌍".into())] && tool_calls.is_empty())
    );
    // A structural snapshot replaces the indexed state, removing stale blocks.
    let corrected = mapper.map(&decode(update("text_end", 0, None, json!([]))));
    assert!(
        matches!(corrected, Some(DriverEvent::Ui(UiEvent::AssistantUpdate { blocks, tool_calls }))
        if blocks.is_empty() && tool_calls.is_empty())
    );
}

#[test]
fn retry_clears_raw_tool_fragments_before_reusing_a_content_index() {
    let mut mapper = RemoteEventMapper::default();
    mapper.map(&decode(json!({"type":"message_update","eventType":"toolcall_delta","contentIndex":0,"delta":"{\"old\":"})));
    mapper.map(&decode(
        json!({"type":"retry_scheduled","attempt":1,"maxRetries":2,"delayMs":1,"error":"retry"}),
    ));
    mapper.map(&decode(update(
        "toolcall_start",
        0,
        None,
        json!([{"type":"toolCall","id":"new","name":"read","arguments":{}}]),
    )));
    let event = mapper.map(&decode(json!({"type":"message_update","eventType":"toolcall_delta","contentIndex":0,"delta":"{\"fresh\":true}"})));
    assert!(
        matches!(event, Some(DriverEvent::Ui(UiEvent::AssistantUpdate { tool_calls, .. }))
        if tool_calls.len() == 1 && tool_calls[0].id == "new" && tool_calls[0].args == json!({"fresh":true}))
    );
}
