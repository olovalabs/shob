use super::*;
use crate::registry::ExtensionRegistry;
use rpi_ai::types::{Api, StopReason};
use rpi_plugin_sdk::{ProviderStreamCompletionFn, ProviderStreamControl, ProviderStreamEmitFn};

#[derive(Default)]
struct PausedRequest {
    released: Mutex<bool>,
    wake: Condvar,
    rejected_after_cancel: AtomicBool,
}

extern "C" fn unused_request(_: StbStringRef, _: *mut StbString, _: *mut c_void) -> i32 {
    1
}

// Ignores the control surface and returns success only when the test releases
// it. The timeout bounds runtime shutdown even if an assertion fails.
extern "C" fn paused_success(
    _: StbStringRef,
    emit: Option<ProviderStreamEmitFn>,
    emit_data: *mut c_void,
    _: *const ProviderStreamControl,
    _: Option<ProviderStreamCompletionFn>,
    _: *mut c_void,
    out: *mut StbString,
    user_data: *mut c_void,
) -> i32 {
    // SAFETY: the test transfers one Arc into this single request invocation.
    let state = unsafe { Arc::from_raw(user_data.cast::<PausedRequest>()) };
    let emit = emit.expect("the host supplies an emit callback");
    for chunk in [
        serde_json::json!({"type":"usage", "usage": {
            "input":3,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":4,
            "cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}
        }}),
        serde_json::json!({"type":"text_delta","contentIndex":0,"delta":"hello"}),
    ] {
        emit(StbString::from_string(chunk.to_string()), emit_data);
    }
    let guard = state.released.lock().unwrap();
    let _wait = state
        .wake
        .wait_timeout_while(guard, Duration::from_secs(3), |released| !*released)
        .unwrap();
    let rc = emit(
        StbString::from_string(r#"{"type":"text_delta","contentIndex":0,"delta":" late"}"#.into()),
        emit_data,
    );
    state.rejected_after_cancel.store(rc != 0, Ordering::SeqCst);
    let message = serde_json::json!({
        "role":"assistant", "content":[{"type":"text","text":"hello late"}],
        "api":"faux", "provider":"probe", "model":"m", "usage":{
            "input":3,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":5,
            "cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}
        }, "stopReason":"stop", "timestamp":0
    });
    // SAFETY: sole writer to the initialized ABI output slot.
    unsafe {
        *out = StbString::from_string(message.to_string());
    }
    0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_finishes_before_an_uncooperative_plugin_and_retains_its_library() {
    let state = Arc::new(PausedRequest::default());
    let record = RegisteredProvider {
        provider_id: "probe".into(),
        base_url: "https://example".into(),
        api_style: "faux".into(),
        request_fn: unused_request,
        stream_fn: Some(paused_success),
        plugin_free_string: rpi_plugin_sdk::callbacks::free_string,
        user_data: Arc::into_raw(Arc::clone(&state)) as *mut c_void,
    };
    let snapshot = Arc::new(ExtensionRegistry::new().snapshot());
    let keepalive = PluginKeepalive::empty();
    let weak = Arc::downgrade(&keepalive);
    let provider = PluggableProvider::new(record, snapshot, keepalive, Handle::current());
    let model = Model::new("m", "M", Api::Faux, "probe", "https://example");
    let options = SimpleStreamOptions::default();
    let scope = Arc::new(rpi_agent::tool_execution_scope::ToolExecutionScope::default());
    let mut stream = scope
        .run(provider.stream_simple(&model, &Context::new(vec![]), &options))
        .await;
    drop(provider);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match stream.next().await {
                Some(AssistantMessageEvent::TextDelta { .. }) => break,
                Some(_) => {}
                None => panic!("stream closed before the first delta"),
            }
        }
    })
    .await
    .expect("the plugin starts");
    options.signal.cancel();
    let terminal = tokio::time::timeout(Duration::from_millis(500), stream.next()).await;
    let library_retained = weak.upgrade().is_some();
    let capacity_retained = tokio::time::timeout(Duration::from_millis(25), scope.wait_idle())
        .await
        .is_err();

    // Release before asserting so a failure cannot strand a blocking pool thread.
    *state.released.lock().unwrap() = true;
    state.wake.notify_all();
    tokio::time::timeout(Duration::from_secs(2), async {
        while weak.upgrade().is_some() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the library is released after the FFI call finishes");

    assert!(library_retained, "the detached request must retain its DLL");
    assert!(
        capacity_retained,
        "an uncooperative provider must retain child capacity until its FFI call returns"
    );
    let terminal = terminal
        .expect("cancellation must not wait for plugin completion")
        .unwrap();
    let AssistantMessageEvent::Error { reason, error } = terminal else {
        panic!("a cancelled run must not report a successful Done");
    };
    assert_eq!(reason, ErrorReason::Aborted);
    assert_eq!(error.stop_reason, StopReason::Aborted);
    assert_eq!(error.content, vec![Content::text("hello")]);
    assert_eq!(error.model, "m");
    assert_eq!(error.provider, "probe");
    assert_eq!(error.usage.input, 3);
    assert_eq!(error.usage.output, 1);
    assert!(state.rejected_after_cancel.load(Ordering::SeqCst));
    assert!(
        stream.next().await.is_none(),
        "no output follows the aborted terminal"
    );
}

#[test]
fn cancellation_wakes_a_wait_already_in_progress() {
    let state = Arc::new(ProviderControlState::default());
    let other = Arc::clone(&state);
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        other.cancel();
    });
    let start = std::time::Instant::now();
    assert!(state.wait(2_000));
    let elapsed = start.elapsed();
    thread.join().unwrap();
    assert!(
        elapsed < Duration::from_secs(1),
        "cancel wakeup took {elapsed:?}"
    );
}

fn mapper() -> StreamMapper {
    let mut mapper = StreamMapper::new(Api::Faux, "probe".into(), "m".into());
    mapper.apply(serde_json::json!({"type":"start"}));
    mapper.take_deferred();
    mapper
}

fn snapshot(content: serde_json::Value) -> serde_json::Value {
    let mut value =
        serde_json::to_value(AssistantMessage::empty(Api::Faux, "probe", "m", 0)).unwrap();
    value["content"] = serde_json::json!([content]);
    value
}

#[test]
fn delta_snapshots_override_text_thinking_and_tool_arguments() {
    let cases = [
        (
            "text_delta",
            serde_json::json!({"type":"text","text":"authoritative"}),
        ),
        (
            "thinking_delta",
            serde_json::json!({"type":"thinking","thinking":"authoritative"}),
        ),
        (
            "toolcall_delta",
            serde_json::json!({"type":"toolCall","id":"c1","name":"echo","arguments":{"value":"authoritative"}}),
        ),
    ];
    for (kind, content) in cases {
        let mut mapper = mapper();
        let partial = snapshot(content);
        let expected: AssistantMessage = serde_json::from_value(partial.clone()).unwrap();
        mapper
            .apply(serde_json::json!({"type":kind,"contentIndex":0,"delta":"x","partial":partial}));
        assert_eq!(mapper.accumulated().content, expected.content, "{kind}");
        if kind == "toolcall_delta" {
            let end = mapper
                .apply(serde_json::json!({"type":"toolcall_end","contentIndex":0}))
                .unwrap();
            let AssistantMessageEvent::ToolCallEnd { tool_call, .. } = end else {
                panic!()
            };
            assert_eq!(tool_call.arguments["value"], "authoritative");
        }
    }
}

#[test]
fn malformed_snapshots_fall_back_to_delta_accumulation() {
    let mut mapper = mapper();
    mapper.apply(
        serde_json::json!({"type":"text_delta","contentIndex":0,"delta":"hello","partial":{}}),
    );
    assert_eq!(mapper.accumulated().content, vec![Content::text("hello")]);
}

#[test]
fn deferred_delta_preserves_the_complete_snapshot() {
    let mut mapper = StreamMapper::new(Api::Faux, "probe".into(), "m".into());
    let partial = snapshot(serde_json::json!({"type":"text","text":"override"}));
    let chunk =
        serde_json::json!({"type":"text_delta","contentIndex":0,"delta":"x","partial":partial});
    assert_eq!(mapper.apply(chunk.clone()).unwrap().type_tag(), "start");
    let deferred = mapper.take_deferred().unwrap();
    assert_eq!(deferred, chunk);
    mapper.apply(deferred);
    assert_eq!(
        mapper.accumulated().content,
        vec![Content::text("override")]
    );
}

#[test]
fn null_snapshot_is_malformed_and_absent_snapshot_uses_the_delta() {
    let mut mapper = mapper();
    for partial in [None, Some(serde_json::Value::Null)] {
        let mut chunk = serde_json::json!({"type":"text_delta","contentIndex":0,"delta":"x"});
        if let Some(partial) = partial {
            chunk["partial"] = partial;
        }
        mapper.apply(chunk);
    }
    assert_eq!(mapper.accumulated().content, vec![Content::text("xx")]);
}

#[tokio::test]
async fn batched_and_unbatched_mappers_agree_after_every_input_prefix() {
    let mut usage = rpi_ai::Usage::zero();
    usage.input = 3;
    let authoritative = snapshot(serde_json::json!({"type":"text","text":"replacement 🐈"}));
    let chunks = vec![
        serde_json::json!({"type":"start"}),
        serde_json::json!({"type":"text_start","contentIndex":0}),
        serde_json::json!({"type":"text_delta","contentIndex":0,"delta":"hello"}),
        serde_json::json!({"type":"text_delta","contentIndex":0,"delta":" 🐈"}),
        serde_json::json!({"type":"text_delta","contentIndex":0,"delta":""}),
        serde_json::json!({"type":"usage","usage":usage}),
        serde_json::json!({"type":"text_delta","contentIndex":0,"delta":"!"}),
        serde_json::json!({"type":"text_delta","contentIndex":0,"delta":"?"}),
        serde_json::json!({"type":"text_delta","contentIndex":0,"delta":"ignored","partial":authoritative}),
        serde_json::json!({"type":"text_delta","contentIndex":0,"delta":" tail"}),
        serde_json::json!({"type":"text_end","contentIndex":0}),
        serde_json::json!({"type":"thinking_start","contentIndex":1}),
        serde_json::json!({"type":"thinking_delta","contentIndex":1,"delta":"reason"}),
        serde_json::json!({"type":"thinking_delta","contentIndex":1,"delta":"ing"}),
        serde_json::json!({"type":"thinking_end","contentIndex":1,"redacted":false}),
        serde_json::json!({"type":"toolcall_start","contentIndex":2,"id":"c1","name":"echo"}),
        serde_json::json!({"type":"toolcall_delta","contentIndex":2,"delta":"{\"text\":"}),
        serde_json::json!({"type":"toolcall_delta","contentIndex":2,"delta":"\"hi\"}"}),
        serde_json::json!({"type":"toolcall_end","contentIndex":2}),
    ];
    fn apply(mapper: &mut StreamMapper, chunk: serde_json::Value) {
        mapper.apply(chunk);
        while let Some(deferred) = mapper.take_deferred() {
            mapper.apply(deferred);
        }
    }
    for prefix in 0..=chunks.len() {
        let mut original = StreamMapper::new(Api::Faux, "probe".into(), "m".into());
        let mut batched = StreamMapper::new(Api::Faux, "probe".into(), "m".into());
        let (tx, mut rx) = queue::channel(32, 65536, Arc::new(ProviderControlState::default()));
        for chunk in &chunks[..prefix] {
            apply(&mut original, chunk.clone());
            tx.send(chunk.clone(), chunk.to_string().len()).unwrap();
        }
        drop(tx);
        while let Some(chunk) = rx.recv().await {
            apply(&mut batched, chunk);
        }
        assert_eq!(
            batched.accumulated(),
            original.accumulated(),
            "prefix {prefix}"
        );
        assert_eq!(
            batched.aborted_message(),
            original.aborted_message(),
            "cancel prefix {prefix}"
        );
    }
}

#[derive(Default)]
struct Burst {
    calls: std::sync::atomic::AtomicUsize,
    finished: AtomicBool,
}
static BURST_FREES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
unsafe extern "C" fn burst_free(raw: StbString) {
    if raw.len != 0 {
        BURST_FREES.fetch_add(1, Ordering::SeqCst);
    }
    unsafe { rpi_plugin_sdk::callbacks::free_string(raw) };
}
extern "C" fn burst_request(
    _: StbStringRef,
    emit: Option<ProviderStreamEmitFn>,
    data: *mut c_void,
    _: *const ProviderStreamControl,
    _: Option<ProviderStreamCompletionFn>,
    _: *mut c_void,
    _: *mut StbString,
    state: *mut c_void,
) -> i32 {
    let state = unsafe { Arc::from_raw(state.cast::<Burst>()) };
    let emit = emit.unwrap();
    let mut usage = rpi_ai::Usage::zero();
    usage.input = 3;
    state.calls.fetch_add(1, Ordering::SeqCst);
    emit(
        StbString::from_string(serde_json::json!({"type":"usage","usage":usage}).to_string()),
        data,
    );
    for _ in 0..1000 {
        state.calls.fetch_add(1, Ordering::SeqCst);
        if emit(
            StbString::from_string(r#"{"type":"text_delta","contentIndex":0,"delta":"x"}"#.into()),
            data,
        ) != 0
        {
            break;
        }
    }
    state.finished.store(true, Ordering::SeqCst);
    1
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saturated_pipeline_cancels_uncommitted_content_and_frees_every_frame() {
    for drop_consumer in [false, true] {
        BURST_FREES.store(0, Ordering::SeqCst);
        let state = Arc::new(Burst::default());
        let record = RegisteredProvider {
            provider_id: "probe".into(),
            base_url: "http://localhost".into(),
            api_style: "faux".into(),
            request_fn: unused_request,
            stream_fn: Some(burst_request),
            plugin_free_string: burst_free,
            user_data: Arc::into_raw(Arc::clone(&state)) as *mut c_void,
        };
        let options = SimpleStreamOptions::default();
        let parent = options.signal.clone();
        let (mut prod, mut stream) =
            rpi_ai::event_stream::create_assistant_message_event_stream_with_limits(
                1, 4096, &parent,
            );
        let mut driver_options = options;
        driver_options.signal = prod.cancellation_token();
        let task = tokio::spawn(async move {
            let snapshot = Arc::new(ExtensionRegistry::new().snapshot());
            let model = Model::new("m", "M", Api::Faux, "probe", "http://localhost");
            let message = drive_streaming_plugin_provider(
                &snapshot,
                &record,
                burst_request,
                Handle::current(),
                ProviderRequestLifetime::new(PluginKeepalive::empty()),
                &model,
                &Context::new(vec![]),
                &driver_options,
                &mut prod,
            )
            .await;
            push_terminal(&mut prod, message).await;
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            // Two bridge dequeues prove the deferred usage frame was applied.
            while state.calls.load(Ordering::SeqCst) < 67 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            !state.finished.load(Ordering::SeqCst),
            "bounded queues must stop the burst"
        );
        if drop_consumer {
            drop(stream);
        } else {
            parent.cancel();
            let terminal = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let event = stream.next().await.unwrap();
                    if event.is_terminal() {
                        break event;
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(terminal.partial().stop_reason, StopReason::Aborted);
            assert!(
                terminal.partial().content.is_empty(),
                "blocked delta must not commit"
            );
            assert_eq!(
                terminal.partial().usage.input,
                3,
                "applied metadata must commit"
            );
        }
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !state.finished.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            BURST_FREES.load(Ordering::SeqCst),
            state.calls.load(Ordering::SeqCst)
        );
    }
}
