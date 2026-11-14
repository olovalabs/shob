//! Provider packages use the same C ABI as tools. This bridge owns a runtime
//! per request by default; opt-in contexts reuse resources until lifecycle shutdown.
//!
//! Available with the optional `provider` feature. Use [`crate::export_provider!`]
//! for single-provider plugins, or [`register`] and [`drive`] when registering
//! multiple providers through one plugin entrypoint.
use crate::{
    PluginApi, ProviderRequestFn, ProviderStreamControl, ProviderStreamEmitFn,
    ProviderStreamRequestFn, StbString, StbStringRef,
};
use rpi_ai::{
    AssistantMessage, AssistantMessageEvent, Context, Model, Provider, SimpleStreamOptions,
    StopReason,
};
use std::{ffi::c_void, sync::Arc, time::Duration};

mod context;
pub use context::{drive_with_context, register_with_context, ContextFactory, ProviderContext};

pub type Factory = fn(&Model, &SimpleStreamOptions) -> Arc<dyn Provider>;

/// Reuse the maintained OpenAI-compatible protocol without importing any
/// vendor credentials from the host process environment. Client construction
/// errors propagate without bypassing the configured transport.
pub fn completions(
    model: &Model,
    options: &SimpleStreamOptions,
    anonymous: bool,
) -> Result<Arc<dyn Provider>, String> {
    let client = rpi_ai::http::build_client(&model.base_url, None, None)?;
    if anonymous && options.api_key.is_none() {
        Ok(Arc::new(rpi_ai::providers::openai_completions::OpenAiCompletionsProvider::with_models_without_auth(model.provider.clone(), client, vec![model.clone()])))
    } else {
        Ok(Arc::new(rpi_ai::providers::openai_completions::OpenAiCompletionsProvider::with_models_without_env_api_key(model.provider.clone(), options.api_key.clone(), client, vec![model.clone()])))
    }
}

/// Declare a catalog and register one implementation with a one-shot fallback.
pub fn register(
    api: &PluginApi,
    id: &str,
    base_url: &str,
    style: &str,
    manifest: serde_json::Value,
    request: ProviderRequestFn,
    stream: ProviderStreamRequestFn,
) -> i32 {
    if let Some(declare) = api.declare {
        let json = serde_json::json!({"providers":[manifest]}).to_string();
        let rc = declare(StbStringRef::from_str(&json));
        if rc != 0 {
            return rc;
        }
    }
    let free = crate::callbacks::free_string;
    if let Some(register) = api.register_streaming_provider {
        register(
            StbStringRef::from_str(id),
            StbStringRef::from_str(base_url),
            StbStringRef::from_str(style),
            stream,
            free,
            std::ptr::null_mut(),
        )
    } else if let Some(register) = api.register_provider {
        register(
            StbStringRef::from_str(id),
            StbStringRef::from_str(base_url),
            StbStringRef::from_str(style),
            request,
            free,
            std::ptr::null_mut(),
        )
    } else {
        1
    }
}

#[derive(serde::Deserialize)]
struct Request {
    model: Model,
    context: Context,
    options: serde_json::Value,
    /// Host capabilities; absent (`null`) on hosts that predate them.
    #[serde(default)]
    capabilities: serde_json::Value,
}

thread_local! {
    static HOST_CAPABILITIES: std::cell::RefCell<serde_json::Value> =
        const { std::cell::RefCell::new(serde_json::Value::Null) };
}

/// The `capabilities` object of the request the bridge is driving, or `null`
/// when the host sent none (a host that predates them) or no request is active.
///
/// It is set only on the request thread, for the whole bridge call, so a
/// factory can read it and hand what it needs to the provider it builds. On any
/// other thread it is `null`. That includes tasks a provider spawns under
/// [`drive_with_context`], which run on the pooled runtime's worker threads, so
/// read it in the factory and capture the value. For example, a provider may
/// only produce `ToolCall::recorded` when `capabilities["recordedToolCalls"]` is
/// at least `1`. The bridge enforces that rule itself from the request, not from
/// this value, so a `null` read on the wrong thread can only make a provider
/// more cautious: on a host without the capability, a final message with a
/// recorded call becomes an error either way.
pub fn host_capabilities() -> serde_json::Value {
    HOST_CAPABILITIES.with(|capabilities| capabilities.borrow().clone())
}

/// Run `f` with [`host_capabilities`] returning `capabilities`.
fn with_host_capabilities<R>(capabilities: &serde_json::Value, f: impl FnOnce() -> R) -> R {
    struct Restore(serde_json::Value);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = std::mem::take(&mut self.0);
            HOST_CAPABILITIES.with(|capabilities| *capabilities.borrow_mut() = previous);
        }
    }
    let _restore = Restore(HOST_CAPABILITIES.with(|current| current.replace(capabilities.clone())));
    f()
}

/// A host that predates `recorded` ignores it and executes the call, so a
/// recorded call must never reach it in a final message that can execute. An
/// error or aborted message executes nothing and keeps its own terminal.
fn refuse_unsupported_recording(request: &Request, message: AssistantMessage) -> AssistantMessage {
    let supported = request.capabilities["recordedToolCalls"]
        .as_u64()
        .is_some_and(|version| version >= 1);
    let recorded = message
        .content
        .iter()
        .any(|block| matches!(block, rpi_ai::Content::ToolCall(call) if call.recorded.is_some()));
    if supported
        || !recorded
        || matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
    {
        return message;
    }
    AssistantMessage {
        usage: message.usage,
        ..AssistantMessage::terminal(
            request.model.api.clone(),
            request.model.provider.clone(),
            request.model.id.clone(),
            StopReason::Error,
            "host does not support recorded tool calls",
            0,
        )
    }
}

fn options(value: &serde_json::Value) -> SimpleStreamOptions {
    let mut options = SimpleStreamOptions::default();
    options.api_key = value["apiKey"].as_str().map(str::to_string);
    options.timeout = value["timeoutMs"].as_u64().map(Duration::from_millis);
    options.stream_idle_timeout = value["streamIdleTimeoutMs"]
        .as_u64()
        .map(Duration::from_millis);
    options.max_retry_delay = value["maxRetryDelayMs"].as_u64().map(Duration::from_millis);
    options.max_retries = value["maxRetries"].as_u64().and_then(|n| n.try_into().ok());
    options.max_tokens = value["maxTokens"].as_u64();
    options.temperature = value["temperature"].as_f64();
    options.session_id = value["sessionId"].as_str().map(str::to_string);
    options.headers = serde_json::from_value(value["headers"].clone()).ok();
    options.metadata = serde_json::from_value(value["metadata"].clone()).ok();
    options.reasoning = value["reasoning"]
        .as_str()
        .and_then(|s| serde_json::from_value(serde_json::json!(s.to_ascii_lowercase())).ok());
    options.thinking_budgets = serde_json::from_value(value["thinkingBudgets"].clone()).ok();
    options.cache_retention = match value["cacheRetention"].as_str() {
        Some("None") => rpi_ai::CacheRetention::None,
        Some("Long") => rpi_ai::CacheRetention::Long,
        _ => rpi_ai::CacheRetention::Short,
    };
    options
}

/// Compare every field without constructing a candidate message or JSON tree.
fn reconstructs_text(
    previous: &AssistantMessage,
    next: &AssistantMessage,
    index: usize,
    delta: &str,
) -> bool {
    let AssistantMessage {
        role,
        content,
        api,
        provider,
        model,
        response_model,
        response_id,
        usage,
        stop_reason,
        deferred,
        error_message,
        raw_stop_reason,
        end_turn,
        timestamp,
    } = previous;
    if role != &next.role
        || api != &next.api
        || provider != &next.provider
        || model != &next.model
        || response_model != &next.response_model
        || response_id != &next.response_id
        || usage != &next.usage
        || stop_reason != &next.stop_reason
        || deferred != &next.deferred
        || error_message != &next.error_message
        || raw_stop_reason != &next.raw_stop_reason
        || end_turn != &next.end_turn
        || timestamp != &next.timestamp
        || content.len() != next.content.len()
    {
        return false;
    }
    let (Some(rpi_ai::Content::Text(before)), Some(rpi_ai::Content::Text(after))) =
        (content.get(index), next.content.get(index))
    else {
        return false;
    };
    let rpi_ai::TextContent {
        kind,
        text,
        text_signature,
    } = before;
    kind == &after.kind
        && text_signature == &after.text_signature
        && after.text.strip_prefix(text) == Some(delta)
        && content
            .iter()
            .zip(&next.content)
            .enumerate()
            .all(|(i, (a, b))| i == index || a == b)
}

fn event_json(
    event: &AssistantMessageEvent,
    previous: &mut Option<(usize, Arc<AssistantMessage>)>,
) -> serde_json::Value {
    let compact = match (event, previous.as_ref()) {
        (
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                partial,
            },
            Some((index, before)),
        ) => index == content_index && reconstructs_text(before, partial, *index, delta),
        _ => false,
    };
    *previous = match event {
        AssistantMessageEvent::TextDelta {
            content_index,
            partial,
            ..
        } => Some((*content_index, Arc::clone(partial))),
        _ => None,
    };
    if compact {
        let AssistantMessageEvent::TextDelta {
            content_index,
            delta,
            ..
        } = event
        else {
            unreachable!()
        };
        return serde_json::json!({"type":"text_delta","contentIndex":content_index,"delta":delta});
    }
    let mut chunk = serde_json::to_value(event).expect("provider event JSON");
    // The SDK uses flat tool-call identity fields.
    let tool = chunk.get("toolCall").cloned().or_else(|| {
        if event.type_tag() != "toolcall_start" {
            return None;
        }
        let index = chunk["contentIndex"].as_u64()? as usize;
        chunk.get("partial")?.get("content")?.get(index).cloned()
    });
    if let Some(tool) = tool {
        for field in ["id", "name", "arguments", "recorded"] {
            if let Some(value) = tool.get(field) {
                chunk[field] = value.clone();
            }
        }
    }
    chunk
}

async fn drive_stream(
    provider: Arc<dyn Provider>,
    request: &Request,
    options: &SimpleStreamOptions,
    emit: Option<ProviderStreamEmitFn>,
    emit_data: *mut c_void,
    control: Option<&ProviderStreamControl>,
) -> AssistantMessage {
    let mut stream = provider
        .stream_simple(&request.model, &request.context, options)
        .await;
    let mut partial = Arc::new(AssistantMessage::empty(
        request.model.api.clone(),
        request.model.provider.clone(),
        request.model.id.clone(),
        0,
    ));
    let mut previous_text = None;
    let mut poll = tokio::time::interval(Duration::from_millis(10));
    let message = loop {
        // Check synchronously too: an already-buffered provider can
        // finish before the interval's initial tick becomes ready.
        if options.signal.is_cancelled()
            || control.is_some_and(|control| (control.is_cancelled)(control.user_data, 0) != 0)
        {
            options.signal.cancel();
            let mut partial = (*partial).clone();
            partial.stop_reason = StopReason::Aborted;
            partial.error_message = Some("provider request was cancelled".into());
            break partial;
        }
        tokio::select! {
            biased;
            _ = poll.tick() => {}
            event = stream.next() => {
                let Some(event) = event else {
                    break stream.result().await.unwrap_or_else(|error| AssistantMessage::terminal(
                        request.model.api.clone(), request.model.provider.clone(), request.model.id.clone(), StopReason::Error, error.to_string(), 0));
                };
                match &event {
                    AssistantMessageEvent::Done { message, .. } => break message.clone(),
                    AssistantMessageEvent::Error { error, .. } => break error.clone(),
                    _ => {},
                }
                partial = match &event {
                    AssistantMessageEvent::Start { partial }
                    | AssistantMessageEvent::TextStart { partial, .. }
                    | AssistantMessageEvent::TextDelta { partial, .. }
                    | AssistantMessageEvent::TextEnd { partial, .. }
                    | AssistantMessageEvent::ThinkingStart { partial, .. }
                    | AssistantMessageEvent::ThinkingDelta { partial, .. }
                    | AssistantMessageEvent::ThinkingEnd { partial, .. }
                    | AssistantMessageEvent::ToolCallStart { partial, .. }
                    | AssistantMessageEvent::ToolCallDelta { partial, .. }
                    | AssistantMessageEvent::ToolCallEnd { partial, .. } => Arc::clone(partial),
                    AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. } => unreachable!(),
                };
                let Some(emit) = emit else { continue; };
                let chunk = event_json(&event, &mut previous_text);
                if emit(StbString::from_string(chunk.to_string()), emit_data) != 0 {
                    options.signal.cancel();
                    let mut partial = (*partial).clone();
                    partial.stop_reason = StopReason::Aborted;
                    break partial;
                }
            }
        }
    };
    refuse_unsupported_recording(request, message)
}

/// Drive a synchronous plugin request without retaining any borrowed ABI data.
///
/// # Safety
/// All arguments must satisfy the SDK request/callback lifetime contract. `out`
/// must be writable; any `control` and callback context must outlive this call.
pub unsafe fn drive(
    factory: Factory,
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
        let Ok(request) = serde_json::from_str::<Request>(unsafe { request.as_str() }) else {
            return 2;
        };
        let options = options(&request.options);
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return 1;
        };
        let control = unsafe { control.as_ref() };
        let message = with_host_capabilities(&request.capabilities, || {
            runtime.block_on(async {
                let provider = factory(&request.model, &options);
                drive_stream(provider, &request, &options, emit, emit_data, control).await
            })
        });
        // Convert via Value to normalize tagged Content's field representation.
        let json = serde_json::to_value(message)
            .expect("assistant message JSON")
            .to_string();
        unsafe {
            *out = StbString::from_string(json);
        }
        0
    })
}

/// Export a provider package using an ordinary Rust Provider factory.
///
/// Requires the `provider` feature. For a multi-provider package, use one
/// [`crate::export_plugin!`] entrypoint and call [`register`] for each provider
/// instead of exporting this macro multiple times.
#[macro_export]
macro_rules! export_provider {
    ($id:expr, $base:expr, $api:expr, $manifest:expr, $factory:path) => {
        extern "C" fn request(
            req: $crate::StbStringRef,
            out: *mut $crate::StbString,
            _: *mut std::ffi::c_void,
        ) -> i32 {
            unsafe {
                $crate::provider::drive(
                    $factory,
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
            _: *mut std::ffi::c_void,
        ) -> i32 {
            unsafe { $crate::provider::drive($factory, req, emit, data, control, out) }
        }
        $crate::export_plugin!(|api| $crate::provider::register(
            api, $id, $base, $api, $manifest, request, stream
        ));
    };
}

#[cfg(test)]
mod bridge_tests;

#[cfg(test)]
mod tests {
    use super::*;
    fn factory(_: &Model, _: &SimpleStreamOptions) -> Arc<dyn Provider> {
        rpi_ai::providers::faux::FauxProvider::new(
            rpi_ai::providers::faux::FauxScript::new()
                .with_tool_call("echo", serde_json::json!({"value":"hello"})),
        )
    }
    extern "C" fn collect(raw: StbString, data: *mut c_void) -> i32 {
        let string = unsafe {
            crate::callbacks::OwnedString::from_raw(raw, Some(crate::callbacks::free_string))
        };
        let value = serde_json::from_str(&string.to_string_lossy()).unwrap();
        // The bridge calls emit synchronously on this request thread only.
        unsafe { &mut *data.cast::<Vec<serde_json::Value>>() }.push(value);
        0
    }
    fn request() -> String {
        serde_json::json!({"model":Model::new("m","M",rpi_ai::Api::Faux,"probe","http://localhost"),
            "context":Context::new(vec![]),"options":{}})
        .to_string()
    }
    #[test]
    fn transport_construction_errors_return_terminal_output_with_or_without_emit() {
        fn factory(
            _: &ProviderContext,
            model: &Model,
            options: &SimpleStreamOptions,
        ) -> Result<Arc<dyn Provider>, String> {
            completions(model, options, false)
        }
        let model = Model::new(
            "m",
            "M",
            rpi_ai::Api::OpenaiCompletions,
            "probe",
            ":invalid",
        );
        assert!(completions(&model, &SimpleStreamOptions::default(), false).is_err());
        let request =
            serde_json::json!({"model":model, "context":Context::new(vec![]), "options":{}})
                .to_string();
        for with_emit in [false, true] {
            let mut chunks = Vec::<serde_json::Value>::new();
            let mut out = StbString::empty();
            assert_eq!(
                unsafe {
                    drive_with_context(
                        factory,
                        std::ptr::null_mut(),
                        StbStringRef::from_str(&request),
                        with_emit.then_some(collect as ProviderStreamEmitFn),
                        (&mut chunks as *mut Vec<serde_json::Value>).cast(),
                        std::ptr::null(),
                        &mut out,
                    )
                },
                0
            );
            let output = unsafe {
                crate::callbacks::OwnedString::from_raw(out, Some(crate::callbacks::free_string))
            };
            let message: AssistantMessage =
                serde_json::from_str(&output.to_string_lossy()).unwrap();
            assert_eq!(message.stop_reason, StopReason::Error);
            assert!(message
                .error_message
                .as_deref()
                .unwrap()
                .contains("invalid target URL"));
            assert!(chunks.is_empty(), "terminal errors use the output slot");
        }
    }

    #[test]
    fn bridge_emits_tool_identity_and_valid_terminal_json() {
        let mut chunks = Vec::<serde_json::Value>::new();
        let mut out = StbString::empty();
        let rc = unsafe {
            drive(
                factory,
                StbStringRef::from_str(&request()),
                Some(collect),
                (&mut chunks as *mut Vec<serde_json::Value>).cast(),
                std::ptr::null(),
                &mut out,
            )
        };
        assert_eq!(rc, 0);
        let output = unsafe {
            crate::callbacks::OwnedString::from_raw(out, Some(crate::callbacks::free_string))
        };
        let message: AssistantMessage = serde_json::from_str(&output.to_string_lossy()).unwrap();
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        for kind in ["toolcall_start", "toolcall_end"] {
            let chunk = chunks
                .iter()
                .find(|chunk| chunk["type"] == kind)
                .expect(kind);
            assert_eq!(chunk["name"], "echo");
            assert!(!chunk["id"].as_str().unwrap().is_empty());
        }
        let end = chunks
            .iter()
            .find(|chunk| chunk["type"] == "toolcall_end")
            .unwrap();
        assert_eq!(end["arguments"]["value"], "hello");
        assert!(!chunks
            .iter()
            .any(|chunk| chunk["type"] == "done" || chunk["type"] == "error"));
    }
    #[test]
    fn bridge_honors_the_host_control_surface() {
        extern "C" fn cancelled(_: *mut c_void, _: u32) -> i32 {
            1
        }
        let control = ProviderStreamControl {
            is_cancelled: cancelled,
            wait_cancelled_ms: cancelled,
            user_data: std::ptr::null_mut(),
        };
        let mut out = StbString::empty();
        assert_eq!(
            unsafe {
                drive(
                    factory,
                    StbStringRef::from_str(&request()),
                    None,
                    std::ptr::null_mut(),
                    &control,
                    &mut out,
                )
            },
            0
        );
        let output = unsafe {
            crate::callbacks::OwnedString::from_raw(out, Some(crate::callbacks::free_string))
        };
        let message: AssistantMessage = serde_json::from_str(&output.to_string_lossy()).unwrap();
        assert_eq!(message.stop_reason, StopReason::Aborted);
    }
    #[test]
    fn options_preserve_provider_controls() {
        let options = options(
            &serde_json::json!({"apiKey":"key","timeoutMs":42,"streamIdleTimeoutMs":11,
            "maxRetries":3,"maxRetryDelayMs":17,"reasoning":"Xhigh","maxTokens":80,
            "headers":{"x-custom":"value"},"cacheRetention":"Long","sessionId":"s"}),
        );
        assert_eq!(options.api_key.as_deref(), Some("key"));
        assert_eq!(options.timeout, Some(Duration::from_millis(42)));
        assert_eq!(options.stream_idle_timeout, Some(Duration::from_millis(11)));
        assert_eq!(options.max_retries, Some(3));
        assert_eq!(options.max_retry_delay, Some(Duration::from_millis(17)));
        assert_eq!(options.reasoning, Some(rpi_ai::ThinkingLevel::Xhigh));
        assert_eq!(options.max_tokens, Some(80));
        assert_eq!(options.session_id.as_deref(), Some("s"));
        assert_eq!(options.headers.unwrap()["x-custom"], "value");
    }

    thread_local! {
        /// What the recorded-call factory saw through [`host_capabilities`].
        /// Per thread: the bridge calls the factory on the request thread, and
        /// tests run in parallel.
        static SEEN_CAPABILITIES: std::cell::RefCell<Vec<serde_json::Value>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    fn recording() -> serde_json::Value {
        serde_json::json!({"version":1,"isError":false,
            "result":{"content":[{"type":"text","text":"ran upstream"}]}})
    }

    /// A provider whose remote agent already ran its one tool call.
    fn recorded_factory(model: &Model, _: &SimpleStreamOptions) -> Arc<dyn Provider> {
        SEEN_CAPABILITIES.with(|seen| seen.borrow_mut().push(host_capabilities()));
        recorded_provider(model, StopReason::ToolUse)
    }

    /// The same provider, but its turn failed after the call ran upstream.
    fn recorded_error_factory(model: &Model, _: &SimpleStreamOptions) -> Arc<dyn Provider> {
        recorded_provider(model, StopReason::Error)
    }

    fn recorded_provider(model: &Model, stop_reason: StopReason) -> Arc<dyn Provider> {
        let mut message = AssistantMessage::empty(
            model.api.clone(),
            model.provider.clone(),
            model.id.clone(),
            0,
        );
        message.stop_reason = stop_reason;
        if stop_reason == StopReason::Error {
            message.error_message = Some("upstream failed".into());
        }
        message.content.push(
            serde_json::from_value(serde_json::json!({"type":"toolCall","id":"c1",
                "name":"shell","arguments":{"command":"ls"},"recorded":recording()}))
            .unwrap(),
        );
        let script = rpi_ai::providers::faux::FauxScript::new();
        script.set_responses(vec![rpi_ai::providers::faux::FauxStep::message(message)]);
        rpi_ai::providers::faux::FauxProvider::new(script)
    }

    fn context_recorded_factory(
        _: &ProviderContext,
        model: &Model,
        options: &SimpleStreamOptions,
    ) -> Result<Arc<dyn Provider>, String> {
        Ok(recorded_factory(model, options))
    }

    /// Drive the recorded-call provider through `drive` (or the context bridge)
    /// for a host that sends `capabilities`, or none at all.
    fn drive_recorded(
        capabilities: Option<serde_json::Value>,
        with_context: bool,
    ) -> (Vec<serde_json::Value>, AssistantMessage) {
        let mut request: serde_json::Value = serde_json::from_str(&request()).unwrap();
        if let Some(capabilities) = capabilities {
            request["capabilities"] = capabilities;
        }
        let request = request.to_string();
        let mut chunks = Vec::<serde_json::Value>::new();
        let mut out = StbString::empty();
        let data = (&mut chunks as *mut Vec<serde_json::Value>).cast();
        let rc = unsafe {
            if with_context {
                drive_with_context(
                    context_recorded_factory,
                    std::ptr::null_mut(),
                    StbStringRef::from_str(&request),
                    Some(collect),
                    data,
                    std::ptr::null(),
                    &mut out,
                )
            } else {
                drive(
                    recorded_factory,
                    StbStringRef::from_str(&request),
                    Some(collect),
                    data,
                    std::ptr::null(),
                    &mut out,
                )
            }
        };
        assert_eq!(rc, 0);
        let output = unsafe {
            crate::callbacks::OwnedString::from_raw(out, Some(crate::callbacks::free_string))
        };
        (
            chunks,
            serde_json::from_str(&output.to_string_lossy()).unwrap(),
        )
    }

    fn has_tool_call(message: &AssistantMessage) -> bool {
        message
            .content
            .iter()
            .any(|block| matches!(block, rpi_ai::Content::ToolCall(_)))
    }

    /// A host from before `recorded` ignores it and runs the call. The bridge
    /// sees no `recordedToolCalls` capability, so it fails the turn instead of
    /// handing that host an executable copy of a call that already ran.
    #[test]
    fn an_old_host_gets_an_error_instead_of_a_recorded_call() {
        for with_context in [false, true] {
            for capabilities in [
                None,
                Some(serde_json::json!({})),
                Some(serde_json::json!({"recordedToolCalls": 0})),
            ] {
                let (_, out) = drive_recorded(capabilities.clone(), with_context);
                assert_eq!(out.stop_reason, StopReason::Error, "{capabilities:?}");
                assert!(!has_tool_call(&out), "{out:?}");
                assert_eq!(
                    out.error_message.as_deref(),
                    Some("host does not support recorded tool calls")
                );
                // The turn's usage is still reported.
                let (_, reference) = drive_recorded(
                    Some(serde_json::json!({"recordedToolCalls": 1})),
                    with_context,
                );
                assert!(reference.usage.total_tokens > 0);
                assert_eq!(out.usage, reference.usage);
            }
        }
    }

    /// A host that decodes recording version 1 or later gets the recording on
    /// the `toolcall_end` chunk and in the final message, and the provider can
    /// read the capability through `host_capabilities`.
    #[test]
    fn a_new_host_gets_the_recording_on_the_end_chunk_and_in_out() {
        for with_context in [false, true] {
            for version in [1, 2] {
                let capabilities = serde_json::json!({ "recordedToolCalls": version });
                SEEN_CAPABILITIES.with(|seen| seen.borrow_mut().clear());
                let (chunks, out) = drive_recorded(Some(capabilities.clone()), with_context);
                SEEN_CAPABILITIES.with(|seen| assert_eq!(*seen.borrow(), vec![capabilities]));
                let end = chunks
                    .iter()
                    .find(|chunk| chunk["type"] == "toolcall_end")
                    .expect("a toolcall_end chunk");
                assert_eq!(end["id"], "c1");
                assert_eq!(end["recorded"], recording());
                assert_eq!(out.stop_reason, StopReason::ToolUse);
                let Some(rpi_ai::Content::ToolCall(call)) = out.content.first() else {
                    panic!("expected the recorded call, got {out:?}");
                };
                assert_eq!(call.recorded, Some(recording()));
            }
        }
        assert_eq!(host_capabilities(), serde_json::Value::Null);
    }

    /// An ordinary call's chunk gains no `recorded` key.
    #[test]
    fn an_ordinary_end_chunk_has_no_recorded_key() {
        let mut chunks = Vec::<serde_json::Value>::new();
        let mut out = StbString::empty();
        let rc = unsafe {
            drive(
                factory,
                StbStringRef::from_str(&request()),
                Some(collect),
                (&mut chunks as *mut Vec<serde_json::Value>).cast(),
                std::ptr::null(),
                &mut out,
            )
        };
        assert_eq!(rc, 0);
        drop(unsafe {
            crate::callbacks::OwnedString::from_raw(out, Some(crate::callbacks::free_string))
        });
        let end = chunks
            .iter()
            .find(|chunk| chunk["type"] == "toolcall_end")
            .unwrap();
        assert!(end.get("recorded").is_none(), "{end}");
    }

    /// A failed turn already executes nothing, so the bridge keeps its own
    /// error rather than replacing it for an old host.
    #[test]
    fn an_old_host_keeps_a_failed_turns_own_error() {
        let mut out = StbString::empty();
        let rc = unsafe {
            drive(
                recorded_error_factory,
                StbStringRef::from_str(&request()),
                None,
                std::ptr::null_mut(),
                std::ptr::null(),
                &mut out,
            )
        };
        assert_eq!(rc, 0);
        let output = unsafe {
            crate::callbacks::OwnedString::from_raw(out, Some(crate::callbacks::free_string))
        };
        let message: AssistantMessage = serde_json::from_str(&output.to_string_lossy()).unwrap();
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(message.error_message.as_deref(), Some("upstream failed"));
    }
}
