//! Wire coverage for recorded tool calls (a streamed call whose side effect
//! already happened upstream). The `recorded` field must survive the message
//! JSON, the stream frames and their reducer, and must be absent from ordinary
//! calls so existing transcripts and providers see no change.

use std::sync::Arc;

use rpi_ai::frames::{reduce_frames, AssistantMessageFrame, AssistantMessageFrameEncoder};
use rpi_ai::types::{Api, AssistantMessage, AssistantMessageEvent, Content, StopReason};
use serde_json::json;

fn recording() -> serde_json::Value {
    json!({
        "version": 1,
        "isError": false,
        "result": {
            "content": [{ "type": "text", "text": "recorded output" }],
            "details": {},
            "terminate": true
        }
    })
}

fn call_json(recorded: Option<serde_json::Value>) -> serde_json::Value {
    let mut call = json!({
        "type": "toolCall",
        "id": "call-1",
        "name": "shell",
        "arguments": { "command": "echo hi" },
    });
    if let Some(recorded) = recorded {
        call["recorded"] = recorded;
    }
    call
}

#[test]
fn recorded_tool_call_round_trips_through_message_json() {
    let content: Content = serde_json::from_value(call_json(Some(recording()))).unwrap();
    let json = serde_json::to_value(&content).unwrap();
    assert_eq!(json["recorded"], recording());
    let back: Content = serde_json::from_value(json).unwrap();
    assert_eq!(back, content);
}

#[test]
fn explicit_null_recording_is_kept_distinct_from_no_recording() {
    // `null` must stay a present-but-invalid recording, never collapse into an
    // ordinary executable call.
    let content: Content = serde_json::from_value(call_json(Some(json!(null)))).unwrap();
    let json = serde_json::to_value(&content).unwrap();
    assert_eq!(json.get("recorded"), Some(&json!(null)));
}

#[test]
fn ordinary_tool_call_json_has_no_recorded_key() {
    let content: Content = serde_json::from_value(call_json(None)).unwrap();
    let json = serde_json::to_value(&content).unwrap();
    assert!(json.get("recorded").is_none());
}

#[test]
fn recorded_tool_call_survives_stream_frames_and_the_reducer() {
    let Content::ToolCall(call) =
        serde_json::from_value::<Content>(call_json(Some(recording()))).unwrap()
    else {
        panic!("expected a tool call");
    };
    let mut base = AssistantMessage::empty(Api::Faux, "external", "model", 0);
    base.stop_reason = StopReason::ToolUse;
    base.content.push(Content::ToolCall(call.clone()));
    // The start snapshot precedes the recording: only `toolcall_end` carries it,
    // so the reducer must take the recording from the end frame.
    let mut started = base.clone();
    started.content = vec![serde_json::from_value(call_json(None)).unwrap()];
    let partial = Arc::new(base.clone());
    let events = [
        AssistantMessageEvent::Start {
            partial: Arc::new(AssistantMessage::empty(Api::Faux, "external", "model", 0)),
        },
        AssistantMessageEvent::ToolCallStart {
            content_index: 0,
            partial: Arc::new(started),
        },
        AssistantMessageEvent::ToolCallEnd {
            content_index: 0,
            tool_call: call,
            partial: Arc::clone(&partial),
        },
    ];

    let mut encoder = AssistantMessageFrameEncoder::new();
    let mut frames = Vec::new();
    for event in &events {
        if let Some(frame) = encoder.encode(event).expect("event encodes") {
            frames.push(frame);
        }
    }
    let wire = serde_json::to_value(&frames).unwrap();
    assert!(
        wire.to_string().contains("\"recorded\""),
        "the toolcall_end frame carries the recording"
    );
    let frames: Vec<AssistantMessageFrame> = serde_json::from_value(wire).unwrap();
    let reduced = reduce_frames(&frames).unwrap().expect("a reduced message");
    let Content::ToolCall(reduced_call) = &reduced.content[0] else {
        panic!("expected a tool call block");
    };
    assert_eq!(
        serde_json::to_value(reduced_call).unwrap()["recorded"],
        recording()
    );
}

#[test]
fn frames_without_a_recording_reduce_to_an_ordinary_call() {
    // Frames written before `recorded` existed have no key. Recovery reads them
    // from storage, so they must reduce to an ordinary, executable call.
    let Content::ToolCall(call) = serde_json::from_value::<Content>(call_json(None)).unwrap()
    else {
        panic!("expected a tool call");
    };
    let mut base = AssistantMessage::empty(Api::Faux, "external", "model", 0);
    base.stop_reason = StopReason::ToolUse;
    base.content.push(Content::ToolCall(call.clone()));
    let partial = Arc::new(base.clone());
    let events = [
        AssistantMessageEvent::Start {
            partial: Arc::new(AssistantMessage::empty(Api::Faux, "external", "model", 0)),
        },
        AssistantMessageEvent::ToolCallStart {
            content_index: 0,
            partial: Arc::clone(&partial),
        },
        AssistantMessageEvent::ToolCallEnd {
            content_index: 0,
            tool_call: call,
            partial,
        },
    ];

    let mut encoder = AssistantMessageFrameEncoder::new();
    let mut frames = Vec::new();
    for event in &events {
        if let Some(frame) = encoder.encode(event).expect("event encodes") {
            frames.push(frame);
        }
    }
    let wire = serde_json::to_value(&frames).unwrap();
    assert!(
        !wire.to_string().contains("\"recorded\""),
        "an ordinary call writes no recorded key"
    );
    let frames: Vec<AssistantMessageFrame> = serde_json::from_value(wire).unwrap();
    let reduced = reduce_frames(&frames).unwrap().expect("a reduced message");
    let Content::ToolCall(reduced_call) = &reduced.content[0] else {
        panic!("expected a tool call block");
    };
    assert_eq!(reduced_call.recorded, None);
}
