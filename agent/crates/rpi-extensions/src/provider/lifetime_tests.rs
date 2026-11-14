use super::*;
use crate::registry::ExtensionRegistry;
use futures::FutureExt;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Paused {
    entered: AtomicBool,
    release: Mutex<bool>,
    wake: Condvar,
}
extern "C" fn paused_request(_: StbStringRef, out: *mut StbString, data: *mut c_void) -> i32 {
    // SAFETY: exactly one request consumes the Arc transferred by this test.
    let state = unsafe { Arc::from_raw(data.cast::<Paused>()) };
    state.entered.store(true, Ordering::Release);
    let _wait = state
        .wake
        .wait_timeout_while(
            state.release.lock().unwrap(),
            Duration::from_secs(5),
            |release| !*release,
        )
        .unwrap();
    // SAFETY: the ABI supplies a valid, exclusively writable output slot.
    unsafe {
        *out = StbString::from_string("{}".into());
    }
    0
}

#[test]
fn runtime_shutdown_keeps_one_shot_provider_and_child_capacity_until_ffi_returns() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let state = Arc::new(Paused::default());
    let keepalive = PluginKeepalive::empty();
    let weak = Arc::downgrade(&keepalive);
    let scope = Arc::new(rpi_agent::tool_execution_scope::ToolExecutionScope::default());
    runtime.block_on(async {
        let provider = PluggableProvider::new(
            RegisteredProvider {
                provider_id: "paused".into(),
                base_url: "https://example".into(),
                api_style: "faux".into(),
                request_fn: paused_request,
                stream_fn: None,
                plugin_free_string: rpi_plugin_sdk::callbacks::free_string,
                user_data: Arc::into_raw(state.clone()) as *mut c_void,
            },
            Arc::new(ExtensionRegistry::new().snapshot()),
            keepalive,
            Handle::current(),
        );
        let model = Model::new(
            "m",
            "M",
            rpi_ai::types::Api::Faux,
            "paused",
            "https://example",
        );
        let _stream = scope
            .run(provider.stream_simple(
                &model,
                &Context::new(vec![]),
                &SimpleStreamOptions::default(),
            ))
            .await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !state.entered.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    });
    runtime.shutdown_background();
    let retained = weak.upgrade().is_some();
    let occupied = scope.wait_idle().now_or_never().is_none();
    *state.release.lock().unwrap() = true;
    state.wake.notify_all();
    let deadline = Instant::now() + Duration::from_secs(2);
    while weak.upgrade().is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        retained,
        "the executing one-shot callback must own its DLL, not the aborted async producer"
    );
    assert!(
        occupied,
        "runtime shutdown must not release live foreign execution capacity"
    );
    assert!(weak.upgrade().is_none());
    assert!(scope.wait_idle().now_or_never().is_some());
}
