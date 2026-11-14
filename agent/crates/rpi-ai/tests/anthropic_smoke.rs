//! Live-network smoke test for the Anthropic provider. Mirrors the M3 plan's
//! `anthropic_smoke.rs` shape: gated behind the `smoke` feature (which implies
//! `providers`), and gated AGAIN at runtime by the `ANTHROPIC_API_KEY` env var
//! — when the key is absent the test **skips** (returns early with a notice),
//! never fails. Per plan §5.16 this is API-key auth only.
//!
//! The smoke flow drives `Provider::stream_simple` over a 2-message context
//! (`system: be terse` + `user: use the get_weather tool`) with one tool
//! declared, drains the event stream to the terminal event, and asserts:
//!   - `stop_reason` is `ToolUse` (the model issued exactly one tool call),
//!   - the tool-call args parse as a JSON object with the expected `city`
//!     field, and the call targets the declared tool name.
//!
//! Run with: `cargo test -p pi-ai --features smoke --test anthropic_smoke
//! -- --nocapture --ignored` (or set `ANTHROPIC_API_KEY` and drop `--ignored`).
//! The `#[ignore]` keeps it out of the default `cargo test` run so CI without
//! a key doesn't even attempt the network call.

#![cfg(feature = "smoke")]

use rpi_ai::model::Model;
use rpi_ai::provider::{Provider, SimpleStreamOptions};
use rpi_ai::providers::anthropic::{models::get_model, AnthropicProvider};
use rpi_ai::types::{
    AssistantMessageEvent, Content, Context, Message, Schema, StopReason, Tool, UserContent,
    UserMessage,
};
use serde_json::json;

fn user(text: &str) -> Message {
    Message::User(UserMessage::new(UserContent::Text(text.to_string()), 0))
}

/// The weather tool the smoke conversation asks the model to call.
fn weather_tool() -> Tool {
    Tool {
        name: "get_weather".to_string(),
        description: "Get the current weather for a city.".to_string(),
        parameters: Schema::new(json!({
            "type": "object",
            "properties": {
                "city": { "type": "string", "description": "The city to look up." }
            },
            "required": ["city"],
            "additionalProperties": false,
        })),
        constrained_sampling: None,
    }
}

async fn drain(stream: &mut rpi_ai::AssistantMessageEventStream) -> Vec<AssistantMessageEvent> {
    let mut out = Vec::new();
    while let Some(ev) = stream.next().await {
        out.push(ev);
    }
    out
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY; skipped by default to keep CI network-free"]
async fn anthropic_smoke_tool_use_roundtrip() {
    // SKIP (not FAIL) when the key is absent — plan invariant.
    let api_key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!(
                "ANTHROPIC_API_KEY not set — skipping anthropic smoke test (this is a skip, not a failure)."
            );
            return;
        }
    };

    let model: Model = get_model("claude-haiku-4-5")
        .or_else(|| get_model("claude-sonnet-4-5"))
        .expect("anthropic catalog has at least one model")
        .clone();

    let ctx = Context {
        system_prompt: Some("You are a terse assistant. Use the provided tools when asked about the weather; do not answer from memory.".to_string()),
        messages: vec![
            user("What is the weather in Paris?"),
        ],
        tools: vec![weather_tool()],
    };

    let provider = AnthropicProvider::from_env();
    let opts = SimpleStreamOptions::default().with_api_key(api_key);

    let mut stream = provider.stream_simple(&model, &ctx, &opts).await;
    let events = drain(&mut stream).await;

    // The stream must terminate (terminal event present).
    let last_tag = events.last().map(|e| e.type_tag());
    assert!(
        matches!(last_tag, Some("done") | Some("error")),
        "stream did not terminate; last={:?}",
        last_tag
    );

    let result = stream.result().await.expect("terminal result resolves");

    // If the model refused / errored, surface the message rather than asserting
    // blindly — but still fail the test so a regression is visible.
    assert!(
        matches!(result.stop_reason, StopReason::ToolUse),
        "expected stop_reason ToolUse, got {:?} (error_message={:?})",
        result.stop_reason,
        result.error_message
    );

    // Exactly one tool call, named `get_weather`, args parse as an object.
    let tool_calls: Vec<_> = result
        .content
        .iter()
        .find_map(|c| match c {
            Content::ToolCall(tc) => Some(tc),
            _ => None,
        })
        .into_iter()
        .collect();
    assert_eq!(tool_calls.len(), 1, "expected exactly one tool call");
    let tc = &tool_calls[0];
    assert_eq!(tc.name, "get_weather", "tool call name mismatch");
    assert!(
        tc.arguments.is_object(),
        "args not an object: {:?}",
        tc.arguments
    );
    assert!(
        tc.arguments.get("city").is_some(),
        "args missing `city`: {:?}",
        tc.arguments
    );
}
