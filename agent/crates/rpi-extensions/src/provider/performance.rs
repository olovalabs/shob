//! Offline fixture shared by the baseline and optimized bridge benchmarks.
use super::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::AtomicUsize;

struct CountingAllocator;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static WIRE: AtomicUsize = AtomicUsize::new(0);

// Run this ignored benchmark alone: allocator counters cover the whole process.
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size(), Ordering::Relaxed);
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) };
    }
}

struct Fixture {
    bytes: usize,
    chunk: usize,
}

#[async_trait::async_trait]
impl Provider for Fixture {
    fn id(&self) -> &str {
        "fixture"
    }
    fn models(&self) -> &[Model] {
        &[]
    }
    async fn stream_simple(
        &self,
        model: &Model,
        _: &Context,
        _: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let (mut prod, stream) = create_assistant_message_event_stream();
        let (bytes, chunk) = (self.bytes, self.chunk);
        let mut message = AssistantMessage::empty(model.api.clone(), "fixture", "m", 42);
        tokio::spawn(async move {
            prod.push(AssistantMessageEvent::Start {
                partial: Arc::new(message.clone()),
            })
            .await;
            message.content.push(Content::text(""));
            prod.push(AssistantMessageEvent::TextStart {
                content_index: 0,
                partial: Arc::new(message.clone()),
            })
            .await;
            for start in (0..bytes).step_by(chunk) {
                let delta = "x".repeat(chunk.min(bytes - start));
                let Content::Text(text) = &mut message.content[0] else {
                    unreachable!()
                };
                text.text.push_str(&delta);
                if !prod
                    .push(AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta,
                        partial: Arc::new(message.clone()),
                    })
                    .await
                {
                    return;
                }
            }
            prod.push(AssistantMessageEvent::TextEnd {
                content_index: 0,
                content: "x".repeat(bytes),
                partial: Arc::new(message.clone()),
            })
            .await;
            message.stop_reason = rpi_ai::StopReason::Stop;
            prod.push(AssistantMessageEvent::Done {
                reason: rpi_ai::DoneReason::Stop,
                message,
            })
            .await;
        });
        stream
    }
}

fn factory(_: &Model, options: &SimpleStreamOptions) -> Arc<dyn Provider> {
    Arc::new(Fixture {
        bytes: options.max_tokens.unwrap_or(4096) as usize,
        chunk: options.temperature.unwrap_or(64.0) as usize,
    })
}

struct Emit {
    callback: rpi_plugin_sdk::ProviderStreamEmitFn,
    data: *mut c_void,
}
extern "C" fn counting_emit(raw: StbString, data: *mut c_void) -> i32 {
    WIRE.fetch_add(raw.len, Ordering::Relaxed);
    let emit = unsafe { &*data.cast::<Emit>() };
    (emit.callback)(raw, emit.data)
}
extern "C" fn request(_: StbStringRef, _: *mut StbString, _: *mut c_void) -> i32 {
    1
}
extern "C" fn streaming(
    req: StbStringRef,
    emit: Option<rpi_plugin_sdk::ProviderStreamEmitFn>,
    data: *mut c_void,
    control: *const rpi_plugin_sdk::ProviderStreamControl,
    _: Option<rpi_plugin_sdk::ProviderStreamCompletionFn>,
    _: *mut c_void,
    out: *mut StbString,
    _: *mut c_void,
) -> i32 {
    let mut relay = emit.map(|callback| Emit { callback, data });
    let (callback, data) = match relay.as_mut() {
        Some(relay) => (
            Some(counting_emit as rpi_plugin_sdk::ProviderStreamEmitFn),
            (relay as *mut Emit).cast(),
        ),
        None => (None, std::ptr::null_mut()),
    };
    unsafe { rpi_plugin_sdk::provider::drive(factory, req, callback, data, control, out) }
}

fn parameter(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .map(|v| v.parse().unwrap())
        .unwrap_or(default)
}

#[test]
#[ignore = "release-build offline benchmark; run alone with --nocapture"]
fn offline_provider_pipeline() {
    let bytes = parameter("RPI_BENCH_BYTES", 4096);
    let chunk = parameter("RPI_BENCH_CHUNK", 64);
    let concurrency = parameter("RPI_BENCH_STREAMS", 1);
    let delay = parameter("RPI_BENCH_DELAY_MS", 0);
    let result_only = parameter("RPI_BENCH_RESULT_ONLY", 0) != 0;
    let sdk_only = parameter("RPI_BENCH_SDK_ONLY", 0) != 0;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let before_allocs = ALLOCS.load(Ordering::Relaxed);
    let before_bytes = BYTES.load(Ordering::Relaxed);
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    let started = std::time::Instant::now();
    runtime.block_on(async {
        let mut tasks = Vec::new();
        for _ in 0..concurrency {
            tasks.push(tokio::spawn(async move {
                let model = Model::new("m", "M", rpi_ai::Api::Faux, "fixture", "http://localhost");
                let options = SimpleStreamOptions {
                    max_tokens: Some(bytes as u64),
                    temperature: Some(chunk as f64),
                    ..Default::default()
                };
                let context = Context::new(vec![]);
                let message = if sdk_only {
                    let json = request_envelope_json(&model, &context, &options);
                    tokio::task::spawn_blocking(move || {
                        let mut out = StbString::empty();
                        assert_eq!(
                            unsafe {
                                rpi_plugin_sdk::provider::drive(
                                    factory,
                                    StbStringRef::from_str(&json),
                                    None,
                                    std::ptr::null_mut(),
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
                        serde_json::from_str::<AssistantMessage>(&owned.to_string_lossy()).unwrap()
                    })
                    .await
                    .unwrap()
                } else {
                    let record = RegisteredProvider {
                        provider_id: "fixture".into(),
                        base_url: "http://localhost".into(),
                        api_style: "faux".into(),
                        request_fn: request,
                        stream_fn: Some(streaming),
                        plugin_free_string: rpi_plugin_sdk::callbacks::free_string,
                        user_data: std::ptr::null_mut(),
                    };
                    let snapshot = Arc::new(crate::ExtensionRegistry::new().snapshot());
                    let provider = PluggableProvider::new(
                        record,
                        snapshot,
                        PluginKeepalive::empty(),
                        Handle::current(),
                    );
                    let mut stream = provider.stream_simple(&model, &context, &options).await;
                    if !result_only {
                        while stream.next().await.is_some() {
                            if delay != 0 {
                                tokio::time::sleep(Duration::from_millis(delay as u64)).await;
                            }
                        }
                    }
                    stream.result().await.unwrap()
                };
                assert_eq!(message.content, vec![Content::text("x".repeat(bytes))]);
                assert_eq!(message.timestamp, 42);
                assert_eq!(message.stop_reason, rpi_ai::StopReason::Stop);
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
    });
    let elapsed = started.elapsed().as_secs_f64();
    let allocations = ALLOCS.load(Ordering::Relaxed) - before_allocs;
    let allocated_bytes = BYTES.load(Ordering::Relaxed) - before_bytes;
    let peak = PEAK.load(Ordering::Relaxed);
    let wire = WIRE.load(Ordering::Relaxed);
    let queue = Some(super::queue::peak_occupancy());
    let queue = serde_json::to_string(&queue).unwrap();
    println!("RPI_BENCH {{\"bytes\":{bytes},\"chunk\":{chunk},\"streams\":{concurrency},\"delay_ms\":{delay},\"result_only\":{result_only},\"sdk_only\":{sdk_only},\"elapsed_s\":{elapsed},\"allocations\":{allocations},\"allocated_bytes\":{allocated_bytes},\"peak_live_bytes\":{peak},\"wire_bytes\":{wire},\"bridge_peak_count_bytes\":{queue}}}");
}
