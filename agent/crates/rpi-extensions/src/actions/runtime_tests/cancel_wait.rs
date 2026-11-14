use super::driver::{consume_execute, noop, schema};
use super::*;
use crate::{PluginKeepalive, PluginToolAdapter, PluginToolHandle};
use rpi_agent::AgentTool;
use rpi_plugin_sdk::{StepHandle, StepResult, ToolPartialCb};
use std::future::Future;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

static PHASE: AtomicUsize = AtomicUsize::new(0);
static ENTERED: AtomicBool = AtomicBool::new(false);
static RELEASE: AtomicBool = AtomicBool::new(false);
fn gate() {
    ENTERED.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !RELEASE.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
}
extern "C" fn late_poll(_: StepHandle, _: Option<ToolPartialCb>, _: *mut c_void) -> StepResult {
    if PHASE.load(Ordering::Relaxed) == 0 {
        gate();
    }
    StepResult::done(StbString::from_string("{}".into()))
}
extern "C" fn late_destroy(_: StepHandle) {
    if PHASE.load(Ordering::Relaxed) == 1 {
        gate();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_sleeps_until_the_driver_returns_and_overrides_a_late_success() {
    for phase in 0..2 {
        PHASE.store(phase, Ordering::Relaxed);
        ENTERED.store(false, Ordering::Release);
        RELEASE.store(false, Ordering::Release);
        let adapter = PluginToolAdapter::new(
            schema(),
            PluginToolHandle {
                execute_fn: consume_execute,
                set_waker_fn: None,
                poll_fn: late_poll,
                cancel_fn: noop,
                destroy_fn: late_destroy,
                plugin_free_string: crate::host_free_string,
                runtime_handle: None,
            },
            PluginKeepalive::empty(),
        );
        let signal = CancellationToken::new();
        let call_signal = signal.clone();
        let polls = Arc::new(AtomicUsize::new(0));
        let count = polls.clone();
        let task = tokio::spawn(async move {
            let run = adapter.execute("late", json!({}), call_signal, Arc::new(|_| {}));
            tokio::pin!(run);
            futures::future::poll_fn(|cx| {
                count.fetch_add(1, Ordering::Relaxed);
                run.as_mut().poll(cx)
            })
            .await
        });
        let entered = tokio::time::timeout(Duration::from_secs(2), async {
            while !ENTERED.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        signal.cancel();
        tokio::time::sleep(Duration::from_millis(25)).await;
        let wakes = polls.load(Ordering::Relaxed);
        RELEASE.store(true, Ordering::Release);
        let result = task.await.unwrap();
        entered.unwrap();
        assert!(wakes < 100 && result.is_err(),
            "phase {phase}: cancelled adapter had {wakes} polls while blocked, then returned {result:?}; cancellation must sleep and must not become success");
    }
}
