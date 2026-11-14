use super::*;
use std::cell::RefCell;

thread_local! { static SCRIPT: RefCell<Vec<AssistantMessageEvent>> = const { RefCell::new(Vec::new()) }; }
struct Script(Vec<AssistantMessageEvent>);
#[async_trait::async_trait]
impl Provider for Script {
    fn id(&self) -> &str {
        "probe"
    }
    fn models(&self) -> &[Model] {
        &[]
    }
    async fn stream_simple(
        &self,
        _: &Model,
        _: &Context,
        _: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let events = self.0.clone();
        let (mut prod, stream) = create_assistant_message_event_stream();
        tokio::spawn(async move {
            for event in events {
                if !prod.push(event).await {
                    break;
                }
            }
        });
        stream
    }
}
fn factory(_: &Model, _: &SimpleStreamOptions) -> Arc<dyn Provider> {
    Arc::new(Script(SCRIPT.with(|script| script.borrow().clone())))
}
extern "C" fn collect(raw: StbString, data: *mut c_void) -> i32 {
    let owned = unsafe {
        rpi_plugin_sdk::callbacks::OwnedString::from_raw(
            raw,
            Some(rpi_plugin_sdk::callbacks::free_string),
        )
    };
    let frames = unsafe { &mut *data.cast::<Vec<serde_json::Value>>() };
    frames.push(serde_json::from_str(&owned.to_string_lossy()).unwrap());
    0
}
fn text(message: &mut AssistantMessage, delta: &str) -> AssistantMessageEvent {
    let Content::Text(text) = &mut message.content[0] else {
        unreachable!()
    };
    text.text.push_str(delta);
    AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: delta.into(),
        partial: Arc::new(message.clone()),
    }
}
fn mapped(mapper: &mut StreamMapper, chunk: serde_json::Value) -> Vec<AssistantMessageEvent> {
    let mut events: Vec<_> = mapper.apply(chunk).into_iter().collect();
    while let Some(chunk) = mapper.take_deferred() {
        events.extend(mapper.apply(chunk));
    }
    events
}
#[test]
fn compact_sdk_frames_match_full_frames_after_every_mapper_prefix() {
    let mut message = AssistantMessage::empty(rpi_ai::Api::Faux, "probe", "m", 42);
    message.content.push(Content::text(""));
    let mut events = vec![
        AssistantMessageEvent::Start {
            partial: Arc::new(message.clone()),
        },
        AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: Arc::new(message.clone()),
        },
    ];
    events.push(text(&mut message, "a"));
    events.push(text(&mut message, "🐈"));
    events.push(text(&mut message, ""));
    message.response_id = Some("response".into());
    message.usage.output = 7;
    events.push(text(&mut message, "!"));
    events.push(text(&mut message, "?"));
    message.content[0] = Content::text("authoritative override");
    events.push(text(&mut message, "!"));
    message.content.push(Content::thinking(""));
    events.push(AssistantMessageEvent::ThinkingStart {
        content_index: 1,
        partial: Arc::new(message.clone()),
    });
    if let Content::Thinking(t) = &mut message.content[1] {
        t.thinking = "hmm".into();
        t.thinking_signature = Some("signature".into());
    }
    events.push(AssistantMessageEvent::ThinkingDelta {
        content_index: 1,
        delta: "hmm".into(),
        partial: Arc::new(message.clone()),
    });
    events.push(text(&mut message, "after thinking"));
    events.push(text(&mut message, "..."));
    message.content.push(Content::text("second"));
    events.push(AssistantMessageEvent::TextDelta {
        content_index: 2,
        delta: "second".into(),
        partial: Arc::new(message.clone()),
    });
    events.push(text(&mut message, "after index switch"));
    let mut complete = message.clone();
    complete.stop_reason = rpi_ai::StopReason::Stop;
    let mut script = events.clone();
    script.push(AssistantMessageEvent::Done {
        reason: rpi_ai::DoneReason::Stop,
        message: complete.clone(),
    });
    SCRIPT.with(|slot| *slot.borrow_mut() = script);
    let request = serde_json::json!({"model":Model::new("m","M",rpi_ai::Api::Faux,"probe","http://localhost"),"context":Context::new(vec![]),"options":{}}).to_string();
    let mut frames = Vec::<serde_json::Value>::new();
    let mut out = StbString::empty();
    assert_eq!(
        unsafe {
            rpi_plugin_sdk::provider::drive(
                factory,
                StbStringRef::from_str(&request),
                Some(collect),
                (&mut frames as *mut Vec<serde_json::Value>).cast(),
                std::ptr::null(),
                &mut out,
            )
        },
        0
    );
    let owned = unsafe {
        rpi_plugin_sdk::callbacks::OwnedString::from_raw(
            out,
            Some(rpi_plugin_sdk::callbacks::free_string),
        )
    };
    assert_eq!(
        serde_json::from_str::<AssistantMessage>(&owned.to_string_lossy()).unwrap(),
        complete
    );
    assert_eq!(frames.len(), events.len());
    assert!(
        frames[2].get("partial").is_some(),
        "first delta is authoritative"
    );
    assert!(frames[3].get("partial").is_none(), "pure append compacts");
    assert!(frames[4].get("partial").is_none(), "empty append compacts");
    assert!(
        frames[5].get("partial").is_some(),
        "metadata changes remain authoritative"
    );
    let mut compact = StreamMapper::new(rpi_ai::Api::Faux, "probe".into(), "m".into());
    let mut legacy = StreamMapper::new(rpi_ai::Api::Faux, "probe".into(), "m".into());
    let mut full_bytes = 0;
    let mut compact_bytes = 0;
    for (event, frame) in events.iter().zip(frames) {
        let full = serde_json::to_value(event).unwrap();
        full_bytes += full.to_string().len();
        compact_bytes += frame.to_string().len();
        assert_eq!(mapped(&mut compact, frame), mapped(&mut legacy, full));
        assert_eq!(compact.accumulated(), legacy.accumulated());
        assert_eq!(compact.aborted_message(), legacy.aborted_message());
    }
    assert!(compact_bytes < full_bytes);
}
