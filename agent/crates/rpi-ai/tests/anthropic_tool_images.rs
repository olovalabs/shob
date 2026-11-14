//! Regression coverage for #58 through the ordinary provider HTTP request path.
#![cfg(feature = "providers")]

use rpi_ai::model::{AnthropicMessagesCompat, StreamingProtocolCompat};
use rpi_ai::providers::anthropic::{build_params::AnthropicImageSource, AnthropicProvider};
use rpi_ai::types::{AssistantMessage, ImageContent, StopReason, ToolResultMessage};
use rpi_ai::{
    Api, Content, Context, InputModality, Message, Model, Provider, Schema, SimpleStreamOptions,
    Tool,
};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

fn image(data: &str, mime_type: &str) -> Content {
    Content::Image(ImageContent {
        kind: Default::default(),
        data: data.into(),
        mime_type: mime_type.into(),
    })
}

fn tool(name: &str) -> Tool {
    Tool {
        name: name.into(),
        description: "synthetic tool".into(),
        parameters: Schema::new(json!({"type": "object", "properties": {}})),
        constrained_sampling: None,
    }
}

// Capture the bytes actually POSTed by the provider, without contacting an
// external service or reading an environment API key. The mock 400 is deliberate:
// this tests request serialization, not response streaming or a live model.
async fn capture_request(content: Vec<Content>, vision: bool, deferred: bool) -> (String, Value) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(10), async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            let (body_start, length) = loop {
                let count = socket.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0, "request ended before headers");
                request.extend_from_slice(&buffer[..count]);
                assert!(request.len() < 128 * 1024, "request unexpectedly large");
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..end]).unwrap();
                    assert!(headers.starts_with("POST /v1/messages HTTP/1.1\r\n"));
                    let length = headers
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .expect("reqwest sends a known-length JSON body")
                        .1
                        .trim()
                        .parse::<usize>()
                        .unwrap();
                    assert!(length < 128 * 1024);
                    break (end + 4, length);
                }
            };
            while request.len() < body_start + length {
                let count = socket.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0, "request ended before body");
                request.extend_from_slice(&buffer[..count]);
            }
            let body = String::from_utf8(request[body_start..body_start + length].to_vec()).unwrap();
            let response = r#"{"error":{"type":"invalid_request_error","message":"synthetic capture complete"}}"#;
            let headers = format!(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
            body
        })
        .await
        .expect("loopback capture timed out")
    });
    let mut model = Model::new(
        "synthetic-vision-model",
        "synthetic vision",
        Api::AnthropicMessages,
        "anthropic",
        format!("http://{address}"),
    );
    model.input = vec![InputModality::Text];
    if vision {
        model.input.push(InputModality::Image);
    }
    model.compat = Some(StreamingProtocolCompat::AnthropicMessages(
        AnthropicMessagesCompat {
            supports_tool_references: Some(deferred),
            ..Default::default()
        },
    ));
    let provider = AnthropicProvider::with_models_without_env_api_key(
        Some("synthetic-not-a-secret".into()),
        reqwest::Client::builder().no_proxy().build().unwrap(),
        vec![model.clone()],
    );
    let mut assistant = AssistantMessage::empty(
        Api::AnthropicMessages,
        "anthropic",
        "synthetic-vision-model",
        0,
    );
    assistant.content = vec![Content::tool_call(
        "tool_synthetic",
        "screenshot",
        json!({}),
    )];
    assistant.stop_reason = StopReason::ToolUse;
    let result = ToolResultMessage {
        role: Default::default(),
        tool_call_id: "tool_synthetic".into(),
        tool_name: "screenshot".into(),
        content,
        details: None,
        usage: None,
        added_tool_names: if deferred {
            vec!["inspect".into()]
        } else {
            vec![]
        },
        is_error: false,
        timestamp: 1,
    };
    let mut context = Context::new(vec![
        Message::Assistant(Box::new(assistant)),
        Message::ToolResult(Box::new(result)),
    ]);
    context.tools = vec![tool("screenshot"), tool("inspect")];
    let options = SimpleStreamOptions {
        timeout: Some(Duration::from_secs(5)),
        max_retries: Some(0),
        ..Default::default()
    };
    let stream = provider.stream_simple(&model, &context, &options).await;
    let terminal = tokio::time::timeout(Duration::from_secs(10), stream.result())
        .await
        .expect("provider did not finish")
        .unwrap();
    assert_eq!(terminal.stop_reason, StopReason::Error);
    let raw = server.await.unwrap();
    let body = serde_json::from_str(&raw).unwrap();
    (raw, body)
}

fn result_block(body: &Value) -> &Value {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .find(|block| block["type"] == "tool_result")
        .expect("captured tool_result")
}

fn image_block(data: &str, mime_type: &str) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": mime_type, "data": data}})
}

#[test]
fn image_source_raw_json_has_one_base64_discriminator() {
    let source = AnthropicImageSource {
        kind: "base64".into(),
        media_type: "image/png".into(),
        data: PNG.into(),
    };
    let raw = serde_json::to_string(&source).unwrap();
    assert_eq!(raw.matches("\"type\":").count(), 1, "{raw}");
    let restored: AnthropicImageSource = serde_json::from_str(&raw).unwrap();
    assert_eq!(restored.kind, "base64");
    assert_eq!(restored.media_type, "image/png");
    assert_eq!(&*restored.data, PNG);
}

#[tokio::test]
async fn vision_tool_result_preserves_mixed_text_image_order_and_bytes() {
    let (raw, body) = capture_request(
        vec![
            Content::text("before"),
            image(PNG, "image/png"),
            Content::text("after"),
        ],
        true,
        false,
    )
    .await;
    assert!(raw.contains(PNG));
    assert!(!raw.contains("(see attached image)"));
    assert_eq!(
        result_block(&body)["content"],
        json!([{"type": "text", "text": "before"}, image_block(PNG, "image/png"), {"type": "text", "text": "after"}])
    );
}

#[tokio::test]
async fn vision_image_only_result_has_no_extra_placeholder() {
    let (raw, body) = capture_request(vec![image(PNG, "image/png")], true, false).await;
    assert!(raw.contains(PNG));
    assert!(!raw.contains("(see attached image)"));
    assert_eq!(
        result_block(&body)["content"],
        json!([image_block(PNG, "image/png")])
    );
}

#[tokio::test]
async fn vision_tool_result_preserves_multiple_images_and_mime_types() {
    let (_, body) = capture_request(
        vec![
            image(PNG, "image/png"),
            Content::text("between"),
            image("anBlZw==", "image/jpeg"),
        ],
        true,
        false,
    )
    .await;
    assert_eq!(
        result_block(&body)["content"],
        json!([image_block(PNG, "image/png"), {"type": "text", "text": "between"}, image_block("anBlZw==", "image/jpeg")])
    );
}

#[tokio::test]
async fn nonvision_tool_result_keeps_explicit_omission_placeholder() {
    let (raw, body) = capture_request(
        vec![
            Content::text("before"),
            image(PNG, "image/png"),
            image(PNG, "image/png"),
            Content::text("after"),
        ],
        false,
        false,
    )
    .await;
    assert!(!raw.contains(PNG));
    assert_eq!(
        result_block(&body)["content"],
        "before\n(tool image omitted: model does not support images)\nafter"
    );
}

#[tokio::test]
async fn deferred_tool_reference_displaces_images_to_siblings_without_data_loss() {
    let (raw, body) = capture_request(
        vec![
            Content::text("before"),
            image(PNG, "image/png"),
            Content::text("after"),
        ],
        true,
        true,
    )
    .await;
    assert!(raw.contains(PNG));
    assert!(!raw.contains("(see attached image)"));
    let content = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_array())
        .find(|blocks| blocks.iter().any(|block| block["type"] == "tool_result"))
        .unwrap();
    assert_eq!(content.len(), 4);
    assert_eq!(content[0]["type"], "tool_result");
    assert_eq!(
        content[0]["content"],
        json!([{"type": "tool_reference", "tool_name": "inspect"}])
    );
    assert_eq!(content[1]["text"], "before");
    assert_eq!(content[2], image_block(PNG, "image/png"));
    assert_eq!(content[3]["text"], "after");
}
