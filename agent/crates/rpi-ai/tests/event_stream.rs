//! Integration tests for the SPSC event stream. Mirrors the
//! `event-stream.ts` test shapes: drain deltas + Done; `result()` resolves to
//! the assembled `AssistantMessage`; `Error` path resolves to the error message.

use rpi_ai::event_stream::create_assistant_message_event_stream;
use rpi_ai::{Api, AssistantMessage, AssistantMessageEvent, DoneReason, ErrorReason, StopReason};
use std::sync::Arc;

fn empty_partial(ts: i64) -> Arc<AssistantMessage> {
    Arc::new(AssistantMessage::empty(Api::Faux, "faux", "faux", ts))
}

#[tokio::test]
async fn drain_deltas_then_done() {
    let (mut prod, mut stream) = create_assistant_message_event_stream();
    let partial = empty_partial(0);

    prod.push(AssistantMessageEvent::Start {
        partial: partial.clone(),
    })
    .await;
    prod.push(AssistantMessageEvent::TextStart {
        content_index: 0,
        partial: partial.clone(),
    })
    .await;
    prod.push(AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: "hi".into(),
        partial: partial.clone(),
    })
    .await;
    prod.push(AssistantMessageEvent::TextEnd {
        content_index: 0,
        content: "hi".into(),
        partial: partial.clone(),
    })
    .await;

    let mut final_msg = (*partial).clone();
    final_msg.stop_reason = StopReason::Stop;
    prod.push(AssistantMessageEvent::Done {
        reason: DoneReason::Stop,
        message: final_msg.clone(),
    })
    .await;

    let mut tags = Vec::new();
    while let Some(ev) = stream.next().await {
        tags.push(ev.type_tag());
    }
    assert_eq!(
        tags,
        vec!["start", "text_start", "text_delta", "text_end", "done"]
    );

    let result = stream.result().await.unwrap();
    assert!(matches!(result.stop_reason, StopReason::Stop));
}

#[tokio::test]
async fn result_resolves_to_error_message() {
    let (mut prod, stream) = create_assistant_message_event_stream();
    let err_msg = AssistantMessage::terminal(
        Api::Faux,
        "faux",
        "faux",
        StopReason::Aborted,
        "cancelled",
        0,
    );
    prod.push(AssistantMessageEvent::Error {
        reason: ErrorReason::Aborted,
        error: err_msg.clone(),
    })
    .await;
    let result = stream.result().await.unwrap();
    assert!(matches!(result.stop_reason, StopReason::Aborted));
    assert_eq!(result.error_message.as_deref(), Some("cancelled"));
}

#[tokio::test]
async fn push_after_terminal_is_noop() {
    let (mut prod, mut stream) = create_assistant_message_event_stream();
    let msg = AssistantMessage::terminal(Api::Faux, "faux", "faux", StopReason::Stop, "", 0);
    assert!(
        prod.push(AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: msg,
        })
        .await
    );
    // Post-terminal push must not deliver an extra event.
    assert!(
        prod.push(AssistantMessageEvent::Start {
            partial: empty_partial(1)
        })
        .await
    );

    let mut tags = Vec::new();
    while let Some(ev) = stream.next().await {
        tags.push(ev.type_tag());
    }
    assert_eq!(tags, vec!["done"]);

    let result = stream.result().await.unwrap();
    assert!(matches!(result.stop_reason, StopReason::Stop));
}

#[tokio::test]
async fn producer_close_yields_recv_error() {
    let (prod, mut stream) = create_assistant_message_event_stream();
    // Drop the producer without ever pushing a terminal event.
    drop(prod);
    // `next()` returns None (channel closed).
    assert!(stream.next().await.is_none());
    // `result()` yields the RecvError (no terminal event ever arrived).
    assert!(stream.result().await.is_err());
}
