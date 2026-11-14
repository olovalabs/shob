//! Integration tests for the faux provider. Mirrors the four shapes from the
//! M1 plan: (a) text reply event sequence + `Done`; (b) tool-call event
//! sequence + `Done` with args parse; (c) abort-after → `Error`/`Aborted`;
//! (d) usage estimate populated.

use rpi_ai::model::Model;
use rpi_ai::provider::{Provider, SimpleStreamOptions};
use rpi_ai::providers::faux::{FauxProvider, FauxScript, FauxStep};
use rpi_ai::types::{
    Api, AssistantMessageEvent, Content, Context, Message, StopReason, UserContent, UserMessage,
};
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn user(text: &str) -> Message {
    Message::User(UserMessage {
        role: rpi_ai::types::UserRole,
        content: UserContent::Text(text.to_string()),
        timestamp: 0,
    })
}

fn ctx(text: &str) -> Context {
    Context {
        system_prompt: None,
        messages: vec![user(text)],
        tools: Vec::new(),
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
async fn text_reply_sequence_and_done() {
    let script = FauxScript::new().with_text("hello world");
    let provider = FauxProvider::new(script);
    let model = provider.default_model().clone();
    let mut s = provider
        .stream_simple(&model, &ctx("hi"), &SimpleStreamOptions::default())
        .await;
    let events = drain(&mut s).await;
    let tags: Vec<&str> = events.iter().map(|e| e.type_tag()).collect();

    // Start, then a text block (start/delta+/end), then Done.
    assert_eq!(tags.first(), Some(&"start"));
    assert_eq!(tags.last(), Some(&"done"));
    assert!(tags.contains(&"text_start"));
    assert!(tags.contains(&"text_delta"));
    assert!(tags.contains(&"text_end"));

    let result = s.result().await.unwrap();
    assert!(matches!(result.stop_reason, StopReason::Stop));
    let text = result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(text, "hello world");
    // Usage estimate populated (non-zero input).
    assert!(result.usage.input > 0);
    assert_eq!(
        provider
            .state()
            .call_count
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

#[tokio::test]
async fn tool_call_sequence_and_args_parse() {
    let script = FauxScript::new().with_tool_call("search", json!({"q":"rust"}));
    let provider = FauxProvider::new(script);
    let model = provider.default_model().clone();
    let mut s = provider
        .stream_simple(&model, &ctx("run it"), &SimpleStreamOptions::default())
        .await;
    let events = drain(&mut s).await;
    let tags: Vec<&str> = events.iter().map(|e| e.type_tag()).collect();

    assert!(tags.contains(&"toolcall_start"));
    assert!(tags.contains(&"toolcall_delta"));
    assert!(tags.contains(&"toolcall_end"));
    assert_eq!(tags.last(), Some(&"done"));

    let result = s.result().await.unwrap();
    assert!(matches!(result.stop_reason, StopReason::ToolUse));
    let tc = result
        .content
        .iter()
        .find_map(|c| match c {
            Content::ToolCall(tc) => Some(tc),
            _ => None,
        })
        .expect("a tool call");
    assert_eq!(tc.name, "search");
    assert_eq!(tc.arguments, json!({"q":"rust"}));
}

#[tokio::test]
async fn abort_yields_aborted_error() {
    // tokens_per_second makes deltas take wall-clock time so the abort timer
    // actually lands mid-stream.
    let script = FauxScript::new()
        .with_text("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        .with_tokens_per_second(5.0)
        .with_abort_after(Duration::from_millis(20));
    let provider = FauxProvider::new(script);
    let model = provider.default_model().clone();
    let mut s = provider
        .stream_simple(&model, &ctx("hi"), &SimpleStreamOptions::default())
        .await;
    let events = drain(&mut s).await;
    let tags: Vec<&str> = events.iter().map(|e| e.type_tag()).collect();

    // Must terminate with an Error event (abort), not Done.
    assert_eq!(tags.last(), Some(&"error"));

    let result = s.result().await.unwrap();
    assert!(
        matches!(result.stop_reason, StopReason::Aborted),
        "got {:?}",
        result.stop_reason
    );
    assert!(result.error_message.is_some());
}

#[tokio::test]
async fn external_signal_aborts() {
    let script = FauxScript::new()
        .with_text("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        .with_tokens_per_second(5.0);
    let provider = FauxProvider::new(script);
    let model = provider.default_model().clone();
    let token = CancellationToken::new();
    let opts = SimpleStreamOptions::default().with_signal(token.clone());
    let mut s = provider.stream_simple(&model, &ctx("hi"), &opts).await;
    // Cancel shortly after streaming starts.
    let t = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        t.cancel();
    });
    let events = drain(&mut s).await;
    let tags: Vec<&str> = events.iter().map(|e| e.type_tag()).collect();
    assert_eq!(tags.last(), Some(&"error"));
    let result = s.result().await.unwrap();
    assert!(matches!(result.stop_reason, StopReason::Aborted));
}

#[tokio::test]
async fn empty_queue_yields_error() {
    let provider = FauxProvider::new(FauxScript::new());
    let model = provider.default_model().clone();
    let mut s = provider
        .stream_simple(&model, &ctx("hi"), &SimpleStreamOptions::default())
        .await;
    let events = drain(&mut s).await;
    let tags: Vec<&str> = events.iter().map(|e| e.type_tag()).collect();
    assert_eq!(tags.last(), Some(&"error"));
    let result = s.result().await.unwrap();
    assert!(matches!(result.stop_reason, StopReason::Error));
    assert_eq!(
        result.error_message.as_deref(),
        Some("No more faux responses queued")
    );
}

#[tokio::test]
async fn factory_step_sees_context() {
    let script = FauxScript::new();
    script.set_responses(vec![FauxStep::factory(|ctx: &Context| {
        let last = ctx
            .messages
            .last()
            .and_then(|m| match m {
                Message::User(u) => match &u.content {
                    UserContent::Text(s) => Some(s.clone()),
                    _ => None,
                },
                _ => None,
            })
            .unwrap_or_default();
        rpi_ai::providers::faux::faux_assistant_message(format!("echo:{last}"), StopReason::Stop)
    })]);
    let provider = FauxProvider::new(script);
    let model = provider.default_model().clone();
    let mut s = provider
        .stream_simple(&model, &ctx("ping"), &SimpleStreamOptions::default())
        .await;
    let _ = drain(&mut s).await;
    let result = s.result().await.unwrap();
    let text = result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(text, "echo:ping");
}

#[test]
fn faux_model_carryover() {
    // Sanity: the default model is a Faux-api model with a 128k window.
    let provider = FauxProvider::new(FauxScript::new());
    let m: &Model = provider.default_model();
    assert!(matches!(m.api, Api::Faux));
    assert_eq!(m.context_window, 128_000);
}
