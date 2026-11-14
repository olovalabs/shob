use super::*;
use rpi_ai::providers::faux::{FauxProvider, FauxScript, FauxStep};

fn fixture(_: &Model, _: &SimpleStreamOptions) -> Arc<dyn Provider> {
    let mut message = AssistantMessage::empty(rpi_ai::Api::Faux, "faux", "m", 42);
    message.content = vec![
        rpi_ai::Content::thinking("hmm"),
        rpi_ai::Content::text("hi"),
        rpi_ai::Content::tool_call("c1", "echo", serde_json::json!({})),
    ];
    message.stop_reason = StopReason::ToolUse;
    let script = FauxScript::new();
    script.set_responses(vec![FauxStep::Message(message)]);
    FauxProvider::new(script)
}

fn request() -> String {
    serde_json::json!({"model":Model::new("m","M",rpi_ai::Api::Faux,"faux","http://localhost"),
        "context":Context::new(vec![]),"options":{}})
    .to_string()
}

struct Collector {
    frames: Vec<String>,
    limit: usize,
    reject: bool,
}
extern "C" fn collect(raw: StbString, data: *mut c_void) -> i32 {
    let owned = unsafe {
        crate::callbacks::OwnedString::from_raw(raw, Some(crate::callbacks::free_string))
    };
    let state = unsafe { &mut *data.cast::<Collector>() };
    state.frames.push(owned.to_string_lossy());
    i32::from(state.reject && state.frames.len() == state.limit)
}
extern "C" fn cancelled(data: *mut c_void, _: u32) -> i32 {
    let state = unsafe { &*data.cast::<Collector>() };
    i32::from(state.frames.len() == state.limit)
}

fn run(state: &mut Collector, emit: bool, cancel: bool) -> AssistantMessage {
    let data = (state as *mut Collector).cast();
    let control = ProviderStreamControl {
        is_cancelled: cancelled,
        wait_cancelled_ms: cancelled,
        user_data: data,
    };
    let mut out = StbString::empty();
    assert_eq!(
        unsafe {
            drive(
                fixture,
                StbStringRef::from_str(&request()),
                emit.then_some(collect),
                data,
                if cancel { &control } else { std::ptr::null() },
                &mut out,
            )
        },
        0
    );
    let owned = unsafe {
        crate::callbacks::OwnedString::from_raw(out, Some(crate::callbacks::free_string))
    };
    serde_json::from_str(&owned.to_string_lossy()).unwrap()
}

#[test]
fn bytes_and_partial_results_match_the_legacy_conversion_for_every_event_family() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let events = runtime.block_on(async {
        let request: Request = serde_json::from_str(&request()).unwrap();
        let mut stream = fixture(&request.model, &SimpleStreamOptions::default())
            .stream_simple(
                &request.model,
                &request.context,
                &SimpleStreamOptions::default(),
            )
            .await;
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            if !event.is_terminal() {
                events.push(event);
            }
        }
        events
    });
    let legacy: Vec<String> = events
        .iter()
        .map(|event| {
            let mut chunk = serde_json::to_value(event).unwrap();
            let tool = chunk.get("toolCall").cloned().or_else(|| {
                if event.type_tag() != "toolcall_start" {
                    return None;
                }
                let index = chunk["contentIndex"].as_u64()? as usize;
                chunk.get("partial")?.get("content")?.get(index).cloned()
            });
            if let Some(tool) = tool {
                for field in ["id", "name", "arguments"] {
                    if let Some(value) = tool.get(field) {
                        chunk[field] = value.clone();
                    }
                }
            }
            chunk.to_string()
        })
        .collect();
    let mut state = Collector {
        frames: vec![],
        limit: usize::MAX,
        reject: false,
    };
    let complete = run(&mut state, true, false);
    assert_eq!(state.frames, legacy);
    state.frames.clear();
    assert_eq!(run(&mut state, false, false), complete);
    assert!(state.frames.is_empty());
    for prefix in 1..=events.len() {
        for reject in [true, false] {
            let mut state = Collector {
                frames: vec![],
                limit: prefix,
                reject,
            };
            let actual = run(&mut state, true, !reject);
            let mut expected = events[prefix - 1].partial().clone();
            expected.stop_reason = StopReason::Aborted;
            if !reject {
                expected.error_message = Some("provider request was cancelled".into());
            }
            assert_eq!(actual, expected, "prefix {prefix}, rejection {reject}");
            assert_eq!(state.frames, legacy[..prefix]);
        }
    }
}

#[test]
fn compaction_requires_exact_text_and_metadata_and_consecutive_deltas() {
    let mut before = AssistantMessage::empty(rpi_ai::Api::Faux, "faux", "m", 42);
    before.content = vec![rpi_ai::Content::text("α"), rpi_ai::Content::thinking("hmm")];
    let mut after = before.clone();
    let rpi_ai::Content::Text(text) = &mut after.content[0] else {
        unreachable!()
    };
    text.text.push_str("🐈");
    assert!(reconstructs_text(&before, &after, 0, "🐈"));
    assert!(reconstructs_text(&before, &before, 0, ""));
    assert!(!reconstructs_text(&before, &after, 0, "α🐈"));
    assert!(!reconstructs_text(&before, &after, 1, "🐈"));
    let mutations: &[fn(&mut AssistantMessage)] = &[
        |m| m.api = rpi_ai::Api::AnthropicMessages,
        |m| m.provider.push('x'),
        |m| m.model.push('x'),
        |m| m.response_model = Some("r".into()),
        |m| m.response_id = Some("id".into()),
        |m| m.timestamp += 1,
        |m| m.usage.output += 1,
        |m| m.stop_reason = StopReason::Stop,
        |m| m.error_message = Some("e".into()),
        |m| m.raw_stop_reason = Some("raw".into()),
        |m| m.end_turn = Some(true),
        |m| m.content[1] = rpi_ai::Content::thinking("changed"),
        |m| m.content.push(rpi_ai::Content::text("extra")),
        |m| {
            if let rpi_ai::Content::Text(t) = &mut m.content[0] {
                t.text_signature = Some("signature".into());
            }
        },
    ];
    for mutate in mutations {
        let mut changed = after.clone();
        mutate(&mut changed);
        assert!(!reconstructs_text(&before, &changed, 0, "🐈"));
    }
    let first = AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: "α".into(),
        partial: Arc::new(before),
    };
    let next = AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: "🐈".into(),
        partial: Arc::new(after),
    };
    let mut previous = None;
    assert!(event_json(&first, &mut previous).get("partial").is_some());
    assert!(event_json(&next, &mut previous).get("partial").is_none());
    event_json(
        &AssistantMessageEvent::Start {
            partial: Arc::new(first.partial().clone()),
        },
        &mut previous,
    );
    assert!(event_json(&next, &mut previous).get("partial").is_some());
}
