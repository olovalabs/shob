use super::driver::{consume_execute, noop, schema};
use super::*;
use crate::{PluginKeepalive, PluginToolAdapter, PluginToolHandle};
use rpi_agent::AgentTool;
use rpi_plugin_sdk::{StepHandle, StepResult, ToolPartialCb};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

static RELEASE: AtomicBool = AtomicBool::new(false);
extern "C" fn streaming_poll(
    _: StepHandle,
    cb: Option<ToolPartialCb>,
    ctx: *mut c_void,
) -> StepResult {
    cb.unwrap()(
        StbString::from_string(
            json!({"content":[{"type":"text","text":"live partial"}]}).to_string(),
        ),
        ctx,
    );
    // A broken host must fail the assertion, not hang its blocking pool forever.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !RELEASE.load(Ordering::Acquire) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    StepResult::done(StbString::from_string("{}".into()))
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partials_arrive_while_the_foreign_poll_is_still_running() {
    RELEASE.store(false, Ordering::Release);
    let adapter = PluginToolAdapter::new(
        schema(),
        PluginToolHandle {
            execute_fn: consume_execute,
            set_waker_fn: None,
            poll_fn: streaming_poll,
            cancel_fn: noop,
            destroy_fn: noop,
            plugin_free_string: crate::host_free_string,
            runtime_handle: None,
        },
        PluginKeepalive::empty(),
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        adapter
            .execute(
                "stream",
                json!({}),
                CancellationToken::new(),
                Arc::new(move |partial| {
                    let _ = tx.send(partial);
                }),
            )
            .await
    });
    let received = tokio::time::timeout(Duration::from_secs(1), rx.recv()).await;
    let still_running = !task.is_finished();
    RELEASE.store(true, Ordering::Release);
    task.await.unwrap().unwrap();
    assert!(still_running);
    let partial = received
        .expect("partial must not be buffered until terminal")
        .unwrap();
    assert!(
        matches!(&partial.content[0], rpi_agent::TextContentOrImage::Text(text) if text.text == "live partial")
    );
}
