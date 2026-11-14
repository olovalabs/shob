use super::*;
use crate::{PluginKeepalive, PluginToolAdapter, PluginToolHandle};
use rpi_agent::{tool_execution_scope::ToolExecutionScope, AgentTool};
use rpi_ai::types::{Schema, Tool};
use rpi_plugin_sdk::{FreeStringFn, StepHandle, StepResult, ToolPartialCb};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

pub(super) fn schema() -> Tool {
    Tool {
        name: "research_alias".into(),
        description: "test".into(),
        parameters: Schema::new(json!({"type":"object"})),
        constrained_sampling: None,
    }
}
pub(super) extern "C" fn consume_execute(
    _id: StbStringRef,
    params: StbString,
    free: Option<FreeStringFn>,
) -> StepHandle {
    // SAFETY: execute receives ownership of params and its producer deallocator.
    drop(unsafe { rpi_plugin_sdk::callbacks::OwnedString::from_raw(params, free) });
    std::ptr::NonNull::dangling().as_ptr()
}
pub(super) extern "C" fn noop(_: StepHandle) {}
extern "C" fn noop_set_waker(_: StepHandle, _: Option<rpi_plugin_sdk::ToolWakeCb>, _: *mut c_void) {
}
extern "C" fn slow_set_waker(
    _: StepHandle,
    cb: Option<rpi_plugin_sdk::ToolWakeCb>,
    _: *mut c_void,
) {
    gate(if cb.is_some() { 3 } else { 4 });
}

static REGISTRATION: AtomicUsize = AtomicUsize::new(0);
extern "C" fn delegate_poll(_: StepHandle, _: Option<ToolPartialCb>, _: *mut c_void) -> StepResult {
    let (rc, _) = call(REGISTRATION.load(Ordering::SeqCst), json!({"op":"start"}));
    StepResult::done(StbString::from_string(
        json!({"content":[{"type":"text","text":rc.to_string()}]}).to_string(),
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_alias_plugin_cannot_use_its_registration_to_delegate() {
    for wake in [false, true] {
        let host = Arc::new(Host::default());
        let root = ActionBridge::new(Handle::current(), host.clone());
        let plugin = root.for_plugin("research");
        REGISTRATION.store(plugin.user_data() as usize, Ordering::SeqCst);
        let adapter = PluginToolAdapter::new(
            schema(),
            PluginToolHandle {
                execute_fn: consume_execute,
                set_waker_fn: if wake { Some(noop_set_waker) } else { None },
                poll_fn: delegate_poll,
                cancel_fn: noop,
                destroy_fn: noop,
                plugin_free_string: crate::host_free_string,
                runtime_handle: Some(plugin.user_data() as usize),
            },
            PluginKeepalive::empty(),
        );
        // Synchronous callbacks inherit a host invocation, so parent calls work
        // without explicitly forwarding a capability through their JSON.
        let parent = adapter
            .execute(
                "parent",
                json!({}),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap();
        assert!(
            matches!(&parent.content[0], rpi_agent::TextContentOrImage::Text(text) if text.text == "0")
        );
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        let scope = Arc::new(ToolExecutionScope::default());
        let child = scope
            .run(adapter.execute(
                "child",
                json!({}),
                CancellationToken::new(),
                Arc::new(|_| {}),
            ))
            .await
            .unwrap();
        assert!(
            matches!(&child.content[0], rpi_agent::TextContentOrImage::Text(text) if text.text == "1")
        );
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    }
}

static PHASE: AtomicUsize = AtomicUsize::new(0);
static ENTERED: AtomicBool = AtomicBool::new(false);
static RELEASE: AtomicBool = AtomicBool::new(false);
static DESTROYED: AtomicUsize = AtomicUsize::new(0);
static LAST_ID: Mutex<String> = Mutex::new(String::new());
fn gate(phase: usize) {
    if PHASE.load(Ordering::SeqCst) == phase {
        ENTERED.store(true, Ordering::SeqCst);
        while !RELEASE.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
extern "C" fn slow_execute(
    id: StbStringRef,
    params: StbString,
    free: Option<FreeStringFn>,
) -> StepHandle {
    let handle = consume_execute(id, params, free);
    gate(0);
    // SAFETY: the driver owns this borrowed ID until execute returns, including
    // when its original async caller has already been dropped.
    *LAST_ID.lock().unwrap() = unsafe { id.as_str() }.to_owned();
    handle
}
extern "C" fn slow_poll(_: StepHandle, _: Option<ToolPartialCb>, _: *mut c_void) -> StepResult {
    gate(1);
    StepResult::done(StbString::from_string("{}".into()))
}
extern "C" fn slow_destroy(_: StepHandle) {
    gate(2);
    DESTROYED.fetch_add(1, Ordering::SeqCst);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_future_retains_owned_id_library_and_cleanup_until_every_foreign_phase_returns() {
    for (wake, phase) in (0..3)
        .map(|phase| (false, phase))
        .chain((0..5).map(|phase| (true, phase)))
    {
        PHASE.store(phase, Ordering::SeqCst);
        ENTERED.store(false, Ordering::SeqCst);
        RELEASE.store(false, Ordering::SeqCst);
        DESTROYED.store(0, Ordering::SeqCst);
        let keepalive = PluginKeepalive::empty();
        let weak = Arc::downgrade(&keepalive);
        let adapter = PluginToolAdapter::new(
            schema(),
            PluginToolHandle {
                execute_fn: slow_execute,
                set_waker_fn: if wake { Some(slow_set_waker) } else { None },
                poll_fn: slow_poll,
                cancel_fn: noop,
                destroy_fn: slow_destroy,
                plugin_free_string: crate::host_free_string,
                runtime_handle: None,
            },
            keepalive,
        );
        let scope = Arc::new(ToolExecutionScope::default());
        let run_scope = scope.clone();
        let task = tokio::spawn(async move {
            let id = String::from("owned-after-drop");
            run_scope
                .run(adapter.execute(&id, json!({}), CancellationToken::new(), Arc::new(|_| {})))
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !ENTERED.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let retained = weak.upgrade().is_some();
        let pending = tokio::time::timeout(Duration::from_millis(25), scope.wait_idle())
            .await
            .is_err();
        // Release even if assertions fail, so the test runtime can shut down.
        RELEASE.store(true, Ordering::SeqCst);
        scope.wait_idle().await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert!(retained, "phase {phase}: driver must retain library");
        assert!(
            pending,
            "phase {phase}: cleanup must not finish ahead of driver"
        );
        assert_eq!(*LAST_ID.lock().unwrap(), "owned-after-drop");
        assert_eq!(DESTROYED.load(Ordering::SeqCst), 1);
    }
}
