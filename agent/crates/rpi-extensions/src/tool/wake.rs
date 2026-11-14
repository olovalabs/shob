//! Wake-capable tools use one owned async driver and short, serialized foreign calls.
use super::*;
use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::{Mutex, OnceLock};
use tokio::sync::Notify;

fn wakes() -> &'static Mutex<HashMap<usize, Arc<Notify>>> {
    static WAKES: OnceLock<Mutex<HashMap<usize, Arc<Notify>>>> = OnceLock::new();
    WAKES.get_or_init(Mutex::default)
}

extern "C" fn wake(data: *mut c_void) {
    let notify = wakes()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(data as usize))
        .cloned();
    if let Some(notify) = notify {
        notify.notify_one();
    }
}

struct WakeRegistration {
    token: usize,
    notify: Arc<Notify>,
}

impl WakeRegistration {
    fn new() -> Self {
        // Tokens never wrap or identify an allocation, so late callbacks cannot
        // dereference freed memory or wake a later invocation.
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        // A CAS loop supports the MSRV without the deprecated fetch_update API.
        let mut token = NEXT.load(Ordering::Relaxed);
        loop {
            let next = token
                .checked_add(1)
                .expect("tool wake token space exhausted");
            match NEXT.compare_exchange_weak(token, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(current) => token = current,
            }
        }
        let notify = Arc::new(Notify::new());
        wakes()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(token, notify.clone());
        Self { token, notify }
    }
}

impl Drop for WakeRegistration {
    fn drop(&mut self) {
        wakes()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.token);
    }
}

struct DriverState {
    functions: PluginToolHandle,
    handle: usize,
    callback: PartialCallbackContext,
    registration: WakeRegistration,
    invocation: Option<Arc<crate::ActionBridge>>,
    _background: Option<rpi_agent::tool_execution_scope::BackgroundToolGuard>,
    _keepalive: Arc<PluginKeepalive>,
}

impl DriverState {
    fn cleanup(&mut self, cancelled: bool) {
        let handle = std::mem::replace(&mut self.handle, 0) as StepHandle;
        if handle.is_null() {
            return;
        }
        if cancelled {
            (self.functions.cancel_fn)(handle);
        }
        (self.functions.set_waker_fn.unwrap())(handle, None, std::ptr::null_mut());
        // Invalidate before destroy, including callbacks made by destroy.
        wakes()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.registration.token);
        (self.functions.destroy_fn)(handle);
    }
}

// The async runtime may abort its own tasks during shutdown. Transfer any live
// handle to a cleanup thread, keeping the entire owner through the last call.
struct Driver(Option<DriverState>);
impl std::ops::Deref for Driver {
    type Target = DriverState;
    fn deref(&self) -> &DriverState {
        self.0.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for Driver {
    fn deref_mut(&mut self) -> &mut DriverState {
        self.0.as_mut().unwrap()
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        let state = self.0.take().unwrap();
        if state.handle == 0 {
            return;
        }
        let retained = Arc::new(Mutex::new(Some(state)));
        let cleanup = retained.clone();
        if std::thread::Builder::new()
            .name("plugin-tool-cleanup".into())
            .spawn(move || {
                let mut state = cleanup
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .unwrap();
                let invocation = state.invocation.clone();
                let _context = invocation.as_ref().map(|bridge| bridge.enter_invocation());
                state.cleanup(true);
            })
            .is_err()
        {
            // Retain the library if the OS cannot start a cleanup thread.
            tracing::error!("cannot start plugin tool cleanup; retaining live tool/library");
            std::mem::forget(retained);
        }
    }
}

#[cfg(test)]
static BLOCKING_ACTIVE: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static BLOCKING_PEAK: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
pub(super) struct BlockingCallGuard;
#[cfg(test)]
impl BlockingCallGuard {
    pub(super) fn enter() -> Self {
        let active = BLOCKING_ACTIVE.fetch_add(1, Ordering::SeqCst) + 1;
        BLOCKING_PEAK.fetch_max(active, Ordering::SeqCst);
        Self
    }
}
#[cfg(test)]
impl Drop for BlockingCallGuard {
    fn drop(&mut self) {
        BLOCKING_ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

// Move the entire owner through each blocking call. There can never be two
// calls using its handle, and no blocking worker is retained while Pending.
async fn call<T: Send + 'static>(
    driver: Driver,
    f: impl FnOnce(&mut Driver) -> T + Send + 'static,
) -> (Driver, T) {
    tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        let _blocking = BlockingCallGuard::enter();
        let mut driver = driver;
        let invocation = driver.invocation.clone();
        let _context = invocation.as_ref().map(|bridge| bridge.enter_invocation());
        let value = match catch_unwind(AssertUnwindSafe(|| f(&mut driver))) {
            Ok(value) => value,
            Err(error) => {
                driver.cleanup(true);
                std::panic::resume_unwind(error);
            }
        };
        (driver, value)
    })
    .await
    .expect("tool driver blocking task panicked")
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl PluginToolAdapter {
    pub(super) async fn execute_wake(
        &self,
        tool_call_id: &str,
        params: Value,
        signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let signal = signal.child_token();
        let _cancel_on_drop = CancelOnDrop(signal.clone());
        let invocation = self
            .handle
            .runtime_handle
            .and_then(crate::ActionBridge::resolve_handle)
            .map(|bridge| {
                bridge.for_invocation(
                    rpi_agent::tool_execution_scope::is_child_execution(),
                    signal.clone(),
                )
            });
        let mut params = inject_tool_context(params, &self.context);
        if invocation.is_some() && params.is_null() {
            params = serde_json::json!({});
        }
        if let (Some(invocation), Some(object)) = (&invocation, params.as_object_mut()) {
            let context = object
                .entry(rpi_plugin_sdk::HOST_CONTEXT_KEY)
                .or_insert_with(|| serde_json::json!({}));
            if !context.is_object() {
                *context = serde_json::json!({});
            }
            context["runtimeHandle"] = serde_json::json!(invocation.user_data() as usize);
            context["runtimeKey"] = serde_json::json!(invocation.invocation_key());
        }
        let public_binding = PublicBindingGuard::capture(&params, invocation.as_deref());
        let params = params.to_string();
        let id = tool_call_id.to_owned();
        let (sender, mut partials) = mpsc::unbounded_channel();
        let (done_tx, mut done_rx) = oneshot::channel();
        let driver = Driver(Some(DriverState {
            functions: self.handle,
            handle: 0,
            callback: PartialCallbackContext {
                sender,
                plugin_free: self.handle.plugin_free_string,
                public_binding,
            },
            registration: WakeRegistration::new(),
            invocation,
            _background: rpi_agent::tool_execution_scope::track_background_tool(),
            _keepalive: self.keepalive.clone(),
        }));
        let result_envelope = self.schema.parameters.as_value()["x-rpi-result-envelope"] == 1;
        tokio::spawn(async move {
            let (mut driver, ()) = call(driver, move |driver| {
                driver.handle = (driver.functions.execute_fn)(
                    StbStringRef::from_str(&id),
                    StbString::from_string(params),
                    Some(host_free_string),
                ) as usize;
                if driver.handle != 0 {
                    (driver.functions.set_waker_fn.unwrap())(
                        driver.handle as StepHandle,
                        Some(wake),
                        driver.registration.token as *mut c_void,
                    );
                }
            })
            .await;
            let terminal = if driver.handle == 0 {
                Err(AgentError::Tool(
                    "plugin execute returned null handle".into(),
                ))
            } else {
                loop {
                    if signal.is_cancelled() {
                        break Err(AgentError::Tool("plugin tool cancelled".into()));
                    }
                    let (next, step) = call(driver, |driver| {
                        let result = (driver.functions.poll_fn)(
                            driver.handle as StepHandle,
                            Some(partial_cb_trampoline),
                            &driver.callback as *const _ as *mut c_void,
                        );
                        // SAFETY: poll transfers ownership to the registered producer free fn.
                        unsafe {
                            take_step_result_with(result, driver.functions.plugin_free_string)
                        }
                    })
                    .await;
                    driver = next;
                    if signal.is_cancelled() {
                        break Err(AgentError::Tool("plugin tool cancelled".into()));
                    }
                    match step {
                        Step::Done(done) => {
                            break driver
                                .callback
                                .public_binding
                                .terminal(done_with_invocation(
                                    &done,
                                    result_envelope,
                                    driver.invocation.as_deref(),
                                ))
                        }
                        Step::Err(error) => {
                            break Err(AgentError::Tool(
                                driver.callback.public_binding.string(&error),
                            ))
                        }
                        Step::Pending(progress) => {
                            if !progress.is_empty() {
                                let _ = driver.callback.sender.send(
                                    driver
                                        .callback
                                        .public_binding
                                        .result(json_to_result(&progress)),
                                );
                            }
                            tokio::select! {
                                _ = driver.registration.notify.notified() => {},
                                _ = signal.cancelled() => {},
                            }
                        }
                    }
                }
            };
            let cancelled = signal.is_cancelled();
            let (_driver, ()) = call(driver, move |driver| {
                driver.cleanup(cancelled);
            })
            .await;
            let _ = done_tx.send(terminal);
        });
        loop {
            tokio::select! {
                Some(partial) = partials.recv() => on_update(partial),
                done = &mut done_rx => {
                    while let Ok(partial) = partials.try_recv() { on_update(partial); }
                    return match done {
                        Ok(Ok(_)) if _cancel_on_drop.0.is_cancelled() => Err(AgentError::Tool("plugin tool cancelled".into())),
                        Ok(result) => result,
                        Err(_) => Err(AgentError::State("plugin tool wake driver failed".into())),
                    };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_plugin_sdk::{StepResult, ToolPartialCb, ToolWakeCb};
    use std::time::Duration;

    #[derive(Default)]
    struct State {
        ready: AtomicBool,
        blocked: AtomicBool,
        library: Mutex<Option<std::sync::Weak<PluginKeepalive>>>,
        polls: AtomicUsize,
        cancels: AtomicUsize,
        destroys: AtomicUsize,
        active: AtomicUsize,
        max_active: AtomicUsize,
        callback: Mutex<Option<(ToolWakeCb, usize)>>,
        wake_during_install: AtomicBool,
        wake_during_poll: AtomicBool,
    }
    static STATES: OnceLock<Mutex<HashMap<String, Arc<State>>>> = OnceLock::new();
    static HANDLES: rpi_plugin_sdk::handles::ToolHandles<Arc<State>> =
        rpi_plugin_sdk::handles::ToolHandles::new();
    fn state(handle: StepHandle) -> Arc<State> {
        HANDLES.get(handle).unwrap().lock().clone()
    }
    fn enter(state: &Arc<State>) -> Active {
        let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
        state.max_active.fetch_max(active, Ordering::SeqCst);
        Active(state.clone())
    }
    struct Active(Arc<State>);
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.active.fetch_sub(1, Ordering::SeqCst);
        }
    }
    extern "C" fn execute(
        id: StbStringRef,
        params: StbString,
        free: Option<FreeStringFn>,
    ) -> StepHandle {
        // SAFETY: borrowed id is valid during execute; params transfers ownership.
        drop(unsafe { OwnedString::from_raw(params, free) });
        let state = STATES
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .remove(unsafe { id.as_str() })
            .unwrap();
        let _active = enter(&state);
        HANDLES.insert(state.clone())
    }
    extern "C" fn set_waker(handle: StepHandle, callback: Option<ToolWakeCb>, data: *mut c_void) {
        let state = state(handle);
        let _active = enter(&state);
        *state.callback.lock().unwrap() = callback.map(|cb| (cb, data as usize));
        if state.wake_during_install.load(Ordering::SeqCst) {
            if let Some(callback) = callback {
                callback(data);
            }
        }
    }
    extern "C" fn poll(handle: StepHandle, _: Option<ToolPartialCb>, _: *mut c_void) -> StepResult {
        let state = state(handle);
        let _active = enter(&state);
        let polls = state.polls.fetch_add(1, Ordering::SeqCst);
        while state.blocked.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        if state.ready.load(Ordering::SeqCst)
            || ((state.wake_during_install.load(Ordering::SeqCst)
                || state.wake_during_poll.load(Ordering::SeqCst))
                && polls > 0)
        {
            StepResult::done(StbString::from_string("{}".into()))
        } else {
            // A wake racing the Pending return must be retained too.
            if state.wake_during_poll.load(Ordering::SeqCst) {
                let (cb, data) = state.callback.lock().unwrap().unwrap();
                cb(data as *mut c_void);
            }
            StepResult::pending(StbString::empty())
        }
    }
    extern "C" fn cancel(handle: StepHandle) {
        let state = state(handle);
        let _active = enter(&state);
        state.cancels.fetch_add(1, Ordering::SeqCst);
    }
    extern "C" fn destroy(handle: StepHandle) {
        let state = state(handle);
        let _active = enter(&state);
        state.destroys.fetch_add(1, Ordering::SeqCst);
        HANDLES.remove(handle);
    }
    fn adapter(state: &Arc<State>) -> (String, Arc<PluginToolAdapter>) {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let id = NEXT.fetch_add(1, Ordering::SeqCst).to_string();
        STATES
            .get_or_init(Mutex::default)
            .lock()
            .unwrap()
            .insert(id.clone(), state.clone());
        let keepalive = PluginKeepalive::empty();
        *state.library.lock().unwrap() = Some(Arc::downgrade(&keepalive));
        (
            id,
            Arc::new(PluginToolAdapter::new(
                Tool {
                    name: "wake".into(),
                    description: "test".into(),
                    parameters: rpi_ai::types::Schema::new(serde_json::json!({})),
                    constrained_sampling: None,
                },
                PluginToolHandle {
                    execute_fn: execute,
                    poll_fn: poll,
                    cancel_fn: cancel,
                    destroy_fn: destroy,
                    plugin_free_string: host_free_string,
                    set_waker_fn: Some(set_waker),
                    runtime_handle: None,
                },
                keepalive,
            )),
        )
    }
    async fn until(mut predicate: impl FnMut() -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !predicate() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }
    fn run(
        state: &Arc<State>,
        signal: CancellationToken,
    ) -> tokio::task::JoinHandle<Result<AgentToolResult, AgentError>> {
        let (id, adapter) = adapter(state);
        tokio::spawn(async move {
            adapter
                .execute(&id, Value::Null, signal, Arc::new(|_| {}))
                .await
        })
    }
    #[tokio::test]
    async fn synchronous_wakes_are_retained_and_calls_are_serialized() {
        for install in [false, true] {
            let state = Arc::new(State::default());
            if install {
                state.wake_during_install.store(true, Ordering::SeqCst);
            } else {
                state.wake_during_poll.store(true, Ordering::SeqCst);
            }
            tokio::time::timeout(
                Duration::from_secs(5),
                run(&state, CancellationToken::new()),
            )
            .await
            .unwrap()
            .unwrap()
            .unwrap();
            assert_eq!(state.polls.load(Ordering::SeqCst), 2);
            assert_eq!(state.max_active.load(Ordering::SeqCst), 1);
            assert_eq!(state.destroys.load(Ordering::SeqCst), 1);
            assert!(state.callback.lock().unwrap().is_none());
        }
    }
    #[tokio::test]
    async fn pending_sleeps_then_external_wake_completes() {
        let state = Arc::new(State::default());
        let task = run(&state, CancellationToken::new());
        until(|| state.polls.load(Ordering::SeqCst) == 1).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(state.polls.load(Ordering::SeqCst), 1);
        state.ready.store(true, Ordering::SeqCst);
        let (cb, data) = state.callback.lock().unwrap().unwrap();
        cb(data as *mut c_void);
        task.await.unwrap().unwrap();
        // Stale notifications after destroy do not access invocation memory.
        cb(data as *mut c_void);
        let next = WakeRegistration::new();
        assert_ne!(next.token, data);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), next.notify.notified())
                .await
                .is_err()
        );
        assert_eq!(state.destroys.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn pending_does_not_occupy_the_only_blocking_worker() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let state = Arc::new(State::default());
            let signal = CancellationToken::new();
            let task = run(&state, signal.clone());
            until(|| state.polls.load(Ordering::SeqCst) == 1).await;
            let marker =
                tokio::time::timeout(Duration::from_secs(1), tokio::task::spawn_blocking(|| 7))
                    .await;
            signal.cancel();
            assert!(task.await.unwrap().is_err());
            assert_eq!(marker.unwrap().unwrap(), 7);
        });
    }

    #[test]
    fn runtime_shutdown_cleans_up_pending_handles() {
        for in_flight in [false, true] {
            let state = Arc::new(State::default());
            state.blocked.store(in_flight, Ordering::SeqCst);
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let _task = run(&state, CancellationToken::new());
                until(|| state.polls.load(Ordering::SeqCst) == 1).await;
            });
            runtime.shutdown_background();
            if in_flight {
                assert_eq!(state.destroys.load(Ordering::SeqCst), 0);
                assert!(state
                    .library
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .upgrade()
                    .is_some());
            }
            state.blocked.store(false, Ordering::SeqCst);
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while state.destroys.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(state.destroys.load(Ordering::SeqCst), 1);
            assert_eq!(state.cancels.load(Ordering::SeqCst), 1);
            assert_eq!(state.max_active.load(Ordering::SeqCst), 1);
            assert!(state.callback.lock().unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn cancellation_and_caller_drop_cleanup_pending_handles_once() {
        for drop_caller in [false, true] {
            let state = Arc::new(State::default());
            let signal = CancellationToken::new();
            let task = run(&state, signal.clone());
            until(|| state.polls.load(Ordering::SeqCst) == 1).await;
            if drop_caller {
                task.abort();
                let _ = task.await;
            } else {
                signal.cancel();
                assert!(task.await.unwrap().is_err());
            }
            until(|| state.destroys.load(Ordering::SeqCst) == 1).await;
            assert_eq!(state.cancels.load(Ordering::SeqCst), 1);
            assert_eq!(state.max_active.load(Ordering::SeqCst), 1);
            assert!(state.callback.lock().unwrap().is_none());
        }
    }

    // Offline comparison on the candidate: legacy uses the unchanged polling
    // driver. Baseline 534d316 predates the optional wake registration ABI.
    #[test]
    #[ignore = "offline pending-population benchmark; run alone with --nocapture"]
    fn pending_population_benchmark() {
        for population in [1, 64, 256] {
            for wake in [false, true] {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .max_blocking_threads(512)
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    assert_eq!(BLOCKING_ACTIVE.load(Ordering::SeqCst), 0);
                    BLOCKING_PEAK.store(0, Ordering::SeqCst);
                    let signal = CancellationToken::new();
                    let started = std::time::Instant::now();
                    let mut states = Vec::new();
                    let mut tasks = Vec::new();
                    for _ in 0..population {
                        let state = Arc::new(State::default());
                        let (id, mut adapter) = adapter(&state);
                        if !wake { Arc::get_mut(&mut adapter).unwrap().handle.set_waker_fn = None; }
                        let signal = signal.clone();
                        tasks.push(tokio::spawn(async move {
                            adapter.execute(&id, Value::Null, signal, Arc::new(|_| {})).await
                        }));
                        states.push(state);
                    }
                    until(|| states.iter().all(|state| state.polls.load(Ordering::SeqCst) > 0)).await;
                    let startup = started.elapsed();
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let polls: usize = states.iter().map(|state| state.polls.load(Ordering::SeqCst)).sum();
                    let blocking_pending = BLOCKING_ACTIVE.load(Ordering::SeqCst);
                    let cancelling = std::time::Instant::now();
                    signal.cancel();
                    for task in tasks { assert!(task.await.unwrap().is_err()); }
                    while BLOCKING_ACTIVE.load(Ordering::SeqCst) != 0 { tokio::task::yield_now().await; }
                    let cancellation = cancelling.elapsed();
                    assert!(states.iter().all(|state| state.destroys.load(Ordering::SeqCst) == 1));
                    println!("tool_pending mode={} population={} startup_ms={:.3} elapsed_ms={:.3} polls={} blocking_current_pending={} blocking_peak={} blocking_after_cancel={} cancel_ms={:.3}",
                        if wake { "wake" } else { "candidate_legacy" }, population,
                        startup.as_secs_f64() * 1000.0, started.elapsed().as_secs_f64() * 1000.0,
                        polls, blocking_pending, BLOCKING_PEAK.load(Ordering::SeqCst),
                        BLOCKING_ACTIVE.load(Ordering::SeqCst), cancellation.as_secs_f64() * 1000.0);
                });
            }
        }
    }
}
