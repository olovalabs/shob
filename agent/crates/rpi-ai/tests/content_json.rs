//! Regression coverage for #59: raw JSON must round-trip without Value normalization.
use rpi_ai::types::{
    Api, AssistantMessage, Content, ImageContent, StopReason, TextContent, ThinkingContent,
    ToolCall, ToolResultMessage, UserContent, UserMessage,
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::json;
use std::fmt::Debug;

fn blocks() -> Vec<Content> {
    vec![
        Content::Text(TextContent {
            kind: Default::default(),
            text: "synthetic text / 中文 / \"quoted\"".into(),
            text_signature: Some("text-signature".into()),
        }),
        Content::Thinking(ThinkingContent {
            kind: Default::default(),
            thinking: "synthetic thinking".into(),
            thinking_signature: Some("thinking-signature".into()),
            redacted: true,
        }),
        Content::Image(ImageContent {
            kind: Default::default(),
            data: "aW1hZ2U=".into(),
            mime_type: "image/png".into(),
        }),
        Content::ToolCall(ToolCall {
            kind: Default::default(),
            id: "call_synthetic".into(),
            name: "screenshot".into(),
            arguments: json!({"region": [1, 2], "nested": {"enabled": true}}),
            thought_signature: Some("tool-signature".into()),
            namespace: Some("synthetic".into()),
            recorded: None,
        }),
    ]
}

fn assert_raw_roundtrip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) {
    let raw = serde_json::to_string(value).unwrap();
    let restored: T = serde_json::from_str(&raw)
        .unwrap_or_else(|error| panic!("raw JSON failed to round-trip: {error}; {raw}"));
    assert_eq!(&restored, value);
    let bytes = serde_json::to_vec(value).unwrap();
    let restored: T = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(&restored, value);
}

#[test]
fn all_content_variants_have_one_discriminator_and_raw_roundtrip() {
    for block in blocks() {
        let raw = serde_json::to_string(&block).unwrap();
        // The fixture's payloads contain no nested `type`, so this catches
        // duplicates before a JSON Value could silently normalize them away.
        assert_eq!(raw.matches("\"type\":").count(), 1, "{raw}");
        assert_raw_roundtrip(&block);
    }
}

#[test]
fn standalone_blocks_keep_their_discriminator_and_match_content_encoding() {
    fn check<T: Serialize + DeserializeOwned + PartialEq + Debug>(
        inner: &T,
        block: &Content,
        kind: &str,
    ) {
        let raw = serde_json::to_string(inner).unwrap();
        assert!(raw.starts_with(&format!("{{\"type\":\"{kind}\",")), "{raw}");
        assert_eq!(raw, serde_json::to_string(block).unwrap());
        assert_raw_roundtrip(inner);
    }
    for block in blocks() {
        match &block {
            Content::Text(inner) => check(inner, &block, "text"),
            Content::Thinking(inner) => check(inner, &block, "thinking"),
            Content::Image(inner) => check(inner, &block, "image"),
            Content::ToolCall(inner) => check(inner, &block, "toolCall"),
        }
    }
}

#[test]
fn user_message_text_and_image_blocks_raw_roundtrip() {
    let content = blocks()
        .into_iter()
        .filter(|block| matches!(block, Content::Text(_) | Content::Image(_)))
        .collect();
    assert_raw_roundtrip(&UserMessage::new(UserContent::Blocks(content), 123));
    assert_raw_roundtrip(&UserMessage::new("plain text", 124));
}

#[test]
fn assistant_message_all_blocks_raw_roundtrip() {
    let mut message =
        AssistantMessage::empty(Api::AnthropicMessages, "anthropic", "synthetic", 123);
    message.content = blocks();
    message.stop_reason = StopReason::ToolUse;
    assert_raw_roundtrip(&message);
}

#[test]
fn assistant_usage_cost_raw_roundtrip() {
    // Costs are token counts times per-token prices, so they often land on
    // f64s whose shortest decimal (`1.0500000000000001e-6` for 7 tokens at
    // $0.15/M) only parses back exactly with serde_json's `float_roundtrip`.
    // The harness compares the terminal assistant with its committed entry.
    let mut message =
        AssistantMessage::empty(Api::AnthropicMessages, "anthropic", "synthetic", 123);
    for tokens in 1..2000 {
        message.usage.cost.input = tokens as f64 * 0.15 / 1_000_000.0;
        assert_raw_roundtrip(&message);
    }
}

#[test]
fn tool_result_mixed_blocks_raw_roundtrip() {
    let message = ToolResultMessage {
        role: Default::default(),
        tool_call_id: "call_synthetic".into(),
        tool_name: "screenshot".into(),
        content: blocks()
            .into_iter()
            .filter(|block| matches!(block, Content::Text(_) | Content::Image(_)))
            .collect(),
        details: Some(json!({"synthetic": true})),
        usage: None,
        added_tool_names: vec![],
        is_error: false,
        timestamp: 123,
    };
    assert_raw_roundtrip(&message);
}

#[test]
fn invalid_content_discriminators_are_rejected() {
    for raw in [
        r#"{"type":"unknown","text":"synthetic"}"#,
        r#"{"text":"missing discriminator"}"#,
        r#"{"type":42,"text":"invalid discriminator"}"#,
        r#"{"type":"text","type":"image","text":"duplicate discriminator"}"#,
    ] {
        assert!(serde_json::from_str::<Content>(raw).is_err(), "{raw}");
    }
    assert!(
        serde_json::from_str::<TextContent>(r#"{"type":"image","text":"wrong marker"}"#).is_err()
    );
}
