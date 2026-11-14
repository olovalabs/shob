//! Opt-in registration-scoped resources, retired by the host lifecycle hooks.
use super::*;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Condvar, Mutex};

#[cfg(test)]
thread_local! {
    // Enabled only on benchmark threads, so parallel unit tests cannot affect counts.
    static RESOURCE_BUILDS: std::cell::Cell<Option<(usize, usize)>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn record_resource_build(runtime: bool) {
    RESOURCE_BUILDS.with(|counts| {
        if let Some((runtimes, clients)) = counts.get() {
            counts.set(Some((
                runtimes + usize::from(runtime),
                clients + usize::from(!runtime),
            )));
        }
    });
}

/// Construct a request-specific provider using registration-scoped resources.
pub type ContextFactory =
    fn(&ProviderContext, &Model, &SimpleStreamOptions) -> Result<Arc<dyn Provider>, String>;

#[derive(Default)]
struct State {
    stopping: bool,
    next_request: u64,
    active: BTreeMap<u64, Box<dyn Fn() + Send>>,
}

#[derive(Default)]
struct Resources {
    runtime: Option<tokio::runtime::Runtime>,
    clients: VecDeque<(String, reqwest::Client)>,
}

/// One runtime and a bounded credential-free HTTP pool per registration.
/// Construct providers per request; API keys and headers never enter this cache.
#[derive(Default)]
pub struct ProviderContext {
    state: Mutex<State>,
    resources: Mutex<Resources>,
    idle: Condvar,
    finishing: Mutex<()>,
}

impl ProviderContext {
    /// Create an idle context; runtime and clients are initialized on demand.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reuse the existing proxy and idle-timeout rules. Failed builds propagate.
    pub fn client(&self, base_url: &str) -> Result<reqwest::Client, String> {
        let url = rpi_ai::http::Url::parse(base_url).map_err(|error| error.to_string())?;
        let proxy = rpi_ai::http::resolve_http_proxy_url_for_target(base_url, None)?;
        let key = format!(
            "{}|{}",
            url.origin().ascii_serialization(),
            proxy.as_ref().map(|url| url.as_str()).unwrap_or("")
        );
        let mut resources = self.resources.lock().map_err(|error| error.to_string())?;
        if self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .stopping
        {
            return Err("provider is shutting down".into());
        }
        if let Some((_, client)) = resources.clients.iter().find(|(cached, _)| cached == &key) {
            return Ok(client.clone());
        }
        let client = rpi_ai::http::build_client(base_url, None, None)?;
        #[cfg(test)]
        record_resource_build(false);
        if resources.clients.len() == 16 {
            resources.clients.pop_front();
        }
        resources.clients.push_back((key, client.clone()));
        Ok(client)
    }

    /// Construct an OpenAI-compatible provider with a pooled client and request credentials.
    pub fn completions(
        &self,
        model: &Model,
        options: &SimpleStreamOptions,
        anonymous: bool,
    ) -> Result<Arc<dyn Provider>, String> {
        let client = self.client(&model.base_url)?;
        if anonymous && options.api_key.is_none() {
            Ok(Arc::new(rpi_ai::providers::openai_completions::OpenAiCompletionsProvider::with_models_without_auth(model.provider.clone(), client, vec![model.clone()])))
        } else {
            Ok(Arc::new(rpi_ai::providers::openai_completions::OpenAiCompletionsProvider::with_models_without_env_api_key(model.provider.clone(), options.api_key.clone(), client, vec![model.clone()])))
        }
    }

    fn enter(
        &self,
        options: &SimpleStreamOptions,
    ) -> Result<(tokio::runtime::Handle, RequestGuard<'_>), String> {
        let mut state = self.state.lock().map_err(|error| error.to_string())?;
        if state.stopping {
            return Err("provider is shutting down".into());
        }
        let id = state.next_request;
        state.next_request += 1;
        let signal = options.signal.clone();
        state.active.insert(id, Box::new(move || signal.cancel()));
        drop(state);
        let request = RequestGuard { context: self, id };
        let mut resources = self.resources.lock().map_err(|error| error.to_string())?;
        if resources.runtime.is_none() {
            resources.runtime = Some(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())?,
            );
            #[cfg(test)]
            record_resource_build(true);
        }
        if self
            .state
            .lock()
            .map_err(|error| error.to_string())?
            .stopping
        {
            return Err("provider is shutting down".into());
        }
        let handle = resources.runtime.as_ref().unwrap().handle().clone();
        Ok((handle, request))
    }

    /// Idempotent and nonblocking: reject new calls and cancel active requests.
    pub fn begin_shutdown(&self) {
        let mut state = self.state.lock().unwrap();
        state.stopping = true;
        for cancel in state.active.values() {
            cancel();
        }
    }

    /// Wait for requests and runtime workers. Call from a blocking cleanup thread.
    pub fn finish_shutdown(&self) {
        let _finishing = self.finishing.lock().unwrap();
        self.begin_shutdown();
        let mut state = self.state.lock().unwrap();
        while !state.active.is_empty() {
            state = self.idle.wait(state).unwrap();
        }
        drop(state);
        let mut resources = self.resources.lock().unwrap();
        let runtime = resources.runtime.take();
        resources.clients.clear();
        drop(resources);
        // No timeout: returning guarantees that all runtime workers have joined.
        drop(runtime);
    }
}

struct RequestGuard<'a> {
    context: &'a ProviderContext,
    id: u64,
}
impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        self.context.state.lock().unwrap().active.remove(&self.id);
        self.context.idle.notify_all();
    }
}

extern "C" fn begin_shutdown(data: *mut c_void) {
    crate::guard_or((), || {
        unsafe { &*data.cast::<ProviderContext>() }.begin_shutdown()
    });
}
extern "C" fn finish_shutdown(data: *mut c_void) -> i32 {
    crate::guard_or(1, || {
        let context = unsafe { &*data.cast::<ProviderContext>() };
        context.finish_shutdown();
        unsafe {
            drop(Box::from_raw(data.cast::<ProviderContext>()));
        }
        0
    })
}

/// Like `register`, with resources owned until the host's final shutdown phase.
/// Older hosts receive null context and use request-scoped resources instead.
pub fn register_with_context(
    api: &PluginApi,
    id: &str,
    base_url: &str,
    style: &str,
    manifest: serde_json::Value,
    request: ProviderRequestFn,
    stream: ProviderStreamRequestFn,
) -> i32 {
    let Some(lifecycle) = api.register_plugin_lifecycle else {
        return super::register(api, id, base_url, style, manifest, request, stream);
    };
    let context = Box::into_raw(Box::new(ProviderContext::new()));
    let rc = lifecycle(begin_shutdown, finish_shutdown, context.cast());
    if rc != 0 {
        unsafe {
            drop(Box::from_raw(context));
        }
        return rc;
    }
    if let Some(declare) = api.declare {
        let json = serde_json::json!({"providers":[manifest]}).to_string();
        let rc = declare(StbStringRef::from_str(&json));
        if rc != 0 {
            return rc;
        }
    }
    if let Some(register) = api.register_streaming_provider {
        register(
            StbStringRef::from_str(id),
            StbStringRef::from_str(base_url),
            StbStringRef::from_str(style),
            stream,
            crate::callbacks::free_string,
            context.cast(),
        )
    } else if let Some(register) = api.register_provider {
        register(
            StbStringRef::from_str(id),
            StbStringRef::from_str(base_url),
            StbStringRef::from_str(style),
            request,
            crate::callbacks::free_string,
            context.cast(),
        )
    } else {
        1
    }
}

/// Drive a context factory, falling back to a per-request runtime on older hosts.
///
/// # Safety
/// A non-null `context` must be the live user data from `register_with_context`.
/// Other arguments obey the same borrowed-data contract as [`super::drive`].
pub unsafe fn drive_with_context(
    factory: ContextFactory,
    context: *mut c_void,
    request: StbStringRef,
    emit: Option<ProviderStreamEmitFn>,
    emit_data: *mut c_void,
    control: *const ProviderStreamControl,
    out: *mut StbString,
) -> i32 {
    crate::guard_or(1, || {
        if out.is_null() {
            return 2;
        }
        let Ok(request) = serde_json::from_str::<super::Request>(unsafe { request.as_str() })
        else {
            return 2;
        };
        let options = super::options(&request.options);
        let control = unsafe { control.as_ref() };
        let scoped = ProviderContext::new();
        let context = unsafe { context.cast::<ProviderContext>().as_ref() };
        let resources = context.unwrap_or(&scoped);
        let future = async {
            match factory(resources, &request.model, &options) {
                Ok(provider) => {
                    super::drive_stream(provider, &request, &options, emit, emit_data, control)
                        .await
                }
                Err(error) => AssistantMessage::terminal(
                    request.model.api.clone(),
                    request.model.provider.clone(),
                    request.model.id.clone(),
                    StopReason::Error,
                    error,
                    0,
                ),
            }
        };
        let message = if let Some(context) = context {
            match context.enter(&options) {
                Ok((runtime, _request)) => {
                    super::with_host_capabilities(&request.capabilities, || {
                        runtime.block_on(future)
                    })
                }
                Err(error) => AssistantMessage::terminal(
                    request.model.api.clone(),
                    request.model.provider.clone(),
                    request.model.id.clone(),
                    StopReason::Error,
                    error,
                    0,
                ),
            }
        } else {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return 1;
            };
            #[cfg(test)]
            record_resource_build(true);
            super::with_host_capabilities(&request.capabilities, || runtime.block_on(future))
        };
        let json = serde_json::to_value(message)
            .expect("assistant message JSON")
            .to_string();
        unsafe {
            *out = StbString::from_string(json);
        }
        0
    })
}

/// Export a provider whose factory uses registration-scoped runtime/HTTP resources.
#[macro_export]
macro_rules! export_provider_with_context {
    ($id:expr, $base:expr, $api:expr, $manifest:expr, $factory:path) => {
        extern "C" fn request(
            req: $crate::StbStringRef,
            out: *mut $crate::StbString,
            context: *mut std::ffi::c_void,
        ) -> i32 {
            unsafe {
                $crate::provider::drive_with_context(
                    $factory,
                    context,
                    req,
                    None,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    out,
                )
            }
        }
        extern "C" fn stream(
            req: $crate::StbStringRef,
            emit: Option<$crate::ProviderStreamEmitFn>,
            data: *mut std::ffi::c_void,
            control: *const $crate::ProviderStreamControl,
            _: Option<$crate::ProviderStreamCompletionFn>,
            _: *mut std::ffi::c_void,
            out: *mut $crate::StbString,
            context: *mut std::ffi::c_void,
        ) -> i32 {
            unsafe {
                $crate::provider::drive_with_context(
                    $factory, context, req, emit, data, control, out,
                )
            }
        }
        $crate::export_plugin!(|api| $crate::provider::register_with_context(
            api, $id, $base, $api, $manifest, request, stream
        ));
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_requests_share_one_lazy_runtime_and_shutdown_rejects_work() {
        let context = ProviderContext::new();
        assert!(context.resources.lock().unwrap().runtime.is_none());
        let options = SimpleStreamOptions::default();
        let (first, request) = context.enter(&options).unwrap();
        let runtime = context.resources.lock().unwrap().runtime.as_ref().unwrap() as *const _;
        drop(request);
        let (_, request) = context.enter(&options).unwrap();
        assert_eq!(
            runtime,
            context.resources.lock().unwrap().runtime.as_ref().unwrap() as *const _
        );
        first.block_on(async { tokio::task::yield_now().await });
        context.begin_shutdown();
        context.begin_shutdown();
        assert!(options.signal.is_cancelled());
        assert!(context.enter(&SimpleStreamOptions::default()).is_err());
        drop(request);
        context.finish_shutdown();
        context.finish_shutdown();
        assert!(context.resources.lock().unwrap().runtime.is_none());
    }

    #[test]
    fn begin_shutdown_does_not_wait_for_resource_construction() {
        let context = Arc::new(ProviderContext::new());
        let resources = context.resources.lock().unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        let other = context.clone();
        let worker = std::thread::spawn(move || {
            other.begin_shutdown();
            done.send(()).unwrap();
        });
        assert!(finished.recv_timeout(Duration::from_secs(1)).is_ok());
        drop(resources);
        worker.join().unwrap();
    }

    #[test]
    fn concurrent_calls_progress_and_finish_waits_for_requests() {
        let context = Arc::new(ProviderContext::new());
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let context = context.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                let (runtime, _request) = context.enter(&SimpleStreamOptions::default()).unwrap();
                barrier.wait();
                barrier.wait();
                runtime.block_on(async { tokio::time::sleep(Duration::from_millis(20)).await });
            }));
        }
        barrier.wait();
        assert_eq!(context.state.lock().unwrap().active.len(), 2);
        barrier.wait();
        context.finish_shutdown();
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(context.state.lock().unwrap().active.is_empty());
    }

    #[test]
    fn client_cache_is_bounded_per_instance_and_errors_propagate() {
        let context = ProviderContext::new();
        assert!(context.client(":invalid").is_err());
        for port in 10000..10017 {
            context
                .client(&format!("http://127.0.0.1:{port}/path"))
                .unwrap();
        }
        assert_eq!(context.resources.lock().unwrap().clients.len(), 16);
        context.client("http://127.0.0.1:10016/other-path").unwrap();
        assert_eq!(context.resources.lock().unwrap().clients.len(), 16);
        let other = ProviderContext::new();
        assert!(other.resources.lock().unwrap().clients.is_empty());
        context.finish_shutdown();
        assert!(context.client("http://127.0.0.1:10016").is_err());
    }

    #[test]
    fn http_connections_are_reused_without_retaining_request_credentials() {
        use std::io::{BufRead, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut socket = std::io::BufReader::new(socket);
            for credential in ["first-secret", "second-secret"] {
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    assert!(socket.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    headers.push_str(&line);
                }
                assert!(headers.contains(&format!("authorization: Bearer {credential}")));
                socket
                    .get_mut()
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .unwrap();
            }
        });
        let context = ProviderContext::new();
        let url = format!("http://{address}");
        let (runtime, request) = context.enter(&SimpleStreamOptions::default()).unwrap();
        runtime.block_on(async {
            for credential in ["first-secret", "second-secret"] {
                let client = context.client(&url).unwrap();
                let response = client
                    .get(&url)
                    .bearer_auth(credential)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.text().await.unwrap(), "ok");
            }
        });
        server.join().unwrap();
        assert_eq!(context.resources.lock().unwrap().clients.len(), 1);
        drop(request);
        context.finish_shutdown();
    }
    #[test]
    #[ignore = "offline resource benchmark; run with --ignored --nocapture"]
    fn benchmark_context_resources() {
        fn factory(
            context: &ProviderContext,
            model: &Model,
            options: &SimpleStreamOptions,
        ) -> Result<Arc<dyn Provider>, String> {
            // Exercise real client creation/cache lookup without network variability.
            let _client = context.client(&model.base_url)?;
            Ok(rpi_ai::providers::faux::FauxProvider::new(
                rpi_ai::providers::faux::FauxScript::new()
                    .with_text(options.api_key.as_deref().unwrap()),
            ))
        }
        fn call(context: usize, account: usize) -> Duration {
            let key = format!("account-{account}");
            let request = serde_json::json!({
                "model": Model::new("m", "M", rpi_ai::Api::Faux, "benchmark", "http://127.0.0.1:9"),
                "context": rpi_ai::Context::new(vec![]),
                "options": {"apiKey": key}
            })
            .to_string();
            let mut out = StbString::empty();
            let start = std::time::Instant::now();
            let rc = unsafe {
                drive_with_context(
                    factory,
                    context as *mut c_void,
                    StbStringRef::from_str(&request),
                    None,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &mut out,
                )
            };
            let elapsed = start.elapsed();
            assert_eq!(rc, 0);
            let owned = unsafe {
                crate::callbacks::OwnedString::from_raw(out, Some(crate::callbacks::free_string))
            };
            let message: AssistantMessage = serde_json::from_str(&owned.to_string_lossy()).unwrap();
            assert!(
                matches!(message.content.as_slice(), [rpi_ai::Content::Text(text)] if text.text == key)
            );
            elapsed
        }
        const REQUESTS: usize = 200;
        const THREADS: usize = 4;
        for concurrent in [false, true] {
            for shared in [false, true] {
                let context = ProviderContext::new();
                let pointer = if shared {
                    &context as *const ProviderContext as usize
                } else {
                    0
                };
                let start = std::time::Instant::now();
                let (mut samples, runtimes, clients) = if concurrent {
                    std::thread::scope(|scope| {
                        let barrier = Arc::new(std::sync::Barrier::new(THREADS));
                        let mut workers = Vec::new();
                        for account in 0..THREADS {
                            let barrier = barrier.clone();
                            workers.push(scope.spawn(move || {
                                RESOURCE_BUILDS.with(|counts| counts.set(Some((0, 0))));
                                barrier.wait();
                                let samples = (0..REQUESTS / THREADS)
                                    .map(|_| call(pointer, account))
                                    .collect::<Vec<_>>();
                                let (runtimes, clients) =
                                    RESOURCE_BUILDS.with(|counts| counts.take().unwrap());
                                (samples, runtimes, clients)
                            }));
                        }
                        let mut samples = Vec::new();
                        let (mut runtimes, mut clients) = (0, 0);
                        for worker in workers {
                            let (mut durations, runtime_count, client_count) =
                                worker.join().unwrap();
                            samples.append(&mut durations);
                            runtimes += runtime_count;
                            clients += client_count;
                        }
                        (samples, runtimes, clients)
                    })
                } else {
                    RESOURCE_BUILDS.with(|counts| counts.set(Some((0, 0))));
                    let samples = (0..REQUESTS)
                        .map(|account| call(pointer, account % THREADS))
                        .collect::<Vec<_>>();
                    let (runtimes, clients) = RESOURCE_BUILDS.with(|counts| counts.take().unwrap());
                    (samples, runtimes, clients)
                };
                let elapsed = start.elapsed();
                let expected = if shared { 1 } else { REQUESTS };
                assert_eq!((runtimes, clients), (expected, expected));
                samples.sort_unstable();
                println!(
                    "{}",
                    serde_json::json!({
                        "benchmark": "context_resources", "fixture": "faux_with_client_cache", "network": false, "shared": shared,
                        "concurrent": concurrent, "requests": REQUESTS,
                        "threads": if concurrent { THREADS } else { 1 },
                        "runtime_builds": runtimes, "client_builds": clients,
                        "elapsed_us": elapsed.as_micros(),
                        "request_p50_us": samples[REQUESTS / 2].as_micros(),
                        "request_p95_us": samples[REQUESTS * 95 / 100].as_micros()
                    })
                );
                context.finish_shutdown();
            }
        }
    }
}
