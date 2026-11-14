use super::driver::{consume_execute, noop, schema};
use super::*;
use crate::{PluginKeepalive, PluginToolAdapter, PluginToolHandle};
use rpi_agent::AgentTool;
use rpi_plugin_sdk::{StepHandle, StepResult, ToolPartialCb};
use std::time::{Duration, Instant};

static START: Mutex<Option<Instant>> = Mutex::new(None);
static POLLS: AtomicUsize = AtomicUsize::new(0);
extern "C" fn pending_poll(_: StepHandle, _: Option<ToolPartialCb>, _: *mut c_void) -> StepResult {
    let count = POLLS.fetch_add(1, Ordering::Relaxed) + 1;
    let elapsed = START
        .lock()
        .unwrap()
        .get_or_insert_with(Instant::now)
        .elapsed();
    if elapsed < Duration::from_millis(25) {
        return StepResult::pending(StbString::empty());
    }
    StepResult::done(StbString::from_string(
        json!({"content":[{"type":"text","text":count.to_string()}]}).to_string(),
    ))
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_nonblocking_pending_plugin_does_not_busy_spin_a_cpu_core() {
    *START.lock().unwrap() = None;
    POLLS.store(0, Ordering::Relaxed);
    let adapter = PluginToolAdapter::new(
        schema(),
        PluginToolHandle {
            execute_fn: consume_execute,
            set_waker_fn: None,
            poll_fn: pending_poll,
            cancel_fn: noop,
            destroy_fn: noop,
            plugin_free_string: crate::host_free_string,
            runtime_handle: None,
        },
        PluginKeepalive::empty(),
    );
    adapter
        .execute(
            "pending",
            json!({}),
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
    let count = POLLS.load(Ordering::Relaxed);
    assert!(
        count <= 100,
        "pending plugin was polled {count} times in 25ms; driver needs backoff"
    );
}
