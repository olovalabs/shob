#![cfg(feature = "provider")]

use rpi_ai::{Api, AssistantMessage, Context, Model, Provider, SimpleStreamOptions, StopReason};
use rpi_plugin_sdk as sdk;
use std::{ffi::c_void, ptr, sync::Arc};

const ID: &str = "sdk-test";
const BASE_URL: &str = "http://localhost";

fn factory(_: &Model, _: &SimpleStreamOptions) -> Arc<dyn Provider> {
    rpi_ai::providers::faux::FauxProvider::new(
        rpi_ai::providers::faux::FauxScript::new().with_text("hello"),
    )
}

sdk::export_provider!(
    ID,
    BASE_URL,
    "faux",
    serde_json::json!({"id": ID, "models": [model()]}),
    factory
);

fn model() -> Model {
    Model::new("m", "M", Api::Faux, ID, BASE_URL)
}

fn request_json() -> String {
    serde_json::json!({"model": model(), "context": Context::new(vec![]), "options": {}})
        .to_string()
}

fn terminal_status(raw: sdk::StbString, free: sdk::FreeStringFn) -> i32 {
    // SAFETY: the request callback transfers its sole owned output and matching
    // producer deallocator to the registrar below.
    let output = unsafe { sdk::callbacks::OwnedString::from_raw(raw, Some(free)) };
    match serde_json::from_str::<AssistantMessage>(&output.to_string_lossy()) {
        Ok(message)
            if message.stop_reason == StopReason::Stop
                && matches!(message.content.as_slice(), [rpi_ai::Content::Text(text)] if text.text == "hello") =>
        {
            0
        }
        _ => 91,
    }
}

extern "C" fn declare(raw: sdk::StbStringRef) -> i32 {
    // SAFETY: register borrows a live JSON string for this synchronous call.
    let value = serde_json::from_str::<serde_json::Value>(unsafe { raw.as_str() });
    match value {
        Ok(value) if value["providers"][0]["id"] == ID => 0,
        _ => 92,
    }
}

extern "C" fn register_one_shot(
    _: sdk::StbStringRef,
    _: sdk::StbStringRef,
    _: sdk::StbStringRef,
    request: sdk::ProviderRequestFn,
    free: sdk::FreeStringFn,
    data: *mut c_void,
) -> i32 {
    let json = request_json();
    let mut out = sdk::StbString::empty();
    let rc = request(sdk::StbStringRef::from_str(&json), &mut out, data);
    if rc != 0 {
        return rc;
    }
    terminal_status(out, free)
}

extern "C" fn collect(raw: sdk::StbString, data: *mut c_void) -> i32 {
    // SAFETY: drive emits SDK-owned strings, transferring each exactly once.
    let output =
        unsafe { sdk::callbacks::OwnedString::from_raw(raw, Some(sdk::callbacks::free_string)) };
    if let Ok(event) = serde_json::from_str::<serde_json::Value>(&output.to_string_lossy()) {
        // SAFETY: the streaming registrar lends this Vec for the synchronous
        // request only; no other callback accesses it concurrently.
        unsafe { &mut *data.cast::<Vec<serde_json::Value>>() }.push(event);
        0
    } else {
        93
    }
}

extern "C" fn register_stream(
    _: sdk::StbStringRef,
    _: sdk::StbStringRef,
    _: sdk::StbStringRef,
    request: sdk::ProviderStreamRequestFn,
    free: sdk::FreeStringFn,
    data: *mut c_void,
) -> i32 {
    let json = request_json();
    let mut out = sdk::StbString::empty();
    let mut events = Vec::<serde_json::Value>::new();
    let rc = request(
        sdk::StbStringRef::from_str(&json),
        Some(collect),
        (&mut events as *mut Vec<serde_json::Value>).cast(),
        ptr::null(),
        None,
        ptr::null_mut(),
        &mut out,
        data,
    );
    if rc != 0 {
        return rc;
    }
    let rc = terminal_status(out, free);
    if rc != 0 {
        return rc;
    }
    if events.iter().any(|event| event["type"] == "text_delta")
        && !events
            .iter()
            .any(|event| event["type"] == "done" || event["type"] == "error")
    {
        0
    } else {
        94
    }
}

extern "C" fn unused_one_shot(
    _: sdk::StbStringRef,
    _: sdk::StbStringRef,
    _: sdk::StbStringRef,
    _: sdk::ProviderRequestFn,
    _: sdk::FreeStringFn,
    _: *mut c_void,
) -> i32 {
    95 // Streaming must take precedence whenever the host supports it.
}

extern "C" fn runtime_action(
    _: u32,
    _: sdk::StbStringRef,
    _: *mut sdk::StbString,
    _: *mut c_void,
) -> i32 {
    1
}

fn api() -> sdk::PluginApi {
    sdk::PluginApi {
        abi_version: sdk::RPI_PLUGIN_ABI_VERSION_UNIFIED,
        struct_size: std::mem::size_of::<sdk::PluginApi>() as u32,
        free_string: sdk::callbacks::free_string,
        register_tool: None,
        register_command: None,
        register_shortcut: None,
        register_flag: None,
        register_provider: None,
        register_message_renderer: None,
        register_markdown_transformer: None,
        register_entry_renderer: None,
        register_event_handler: None,
        register_resources_discover: None,
        runtime_action,
        dispatch_event: None,
        user_data: ptr::null_mut(),
        declare: Some(declare),
        register_before_agent_start: None,
        register_oauth_provider: None,
        register_streaming_provider: None,
        register_tool_with_wake: None,
        register_plugin_lifecycle: None,
        register_tool_prompt: None,
        register_oauth_provider_cancellable: None,
        register_tool_result: None,
    }
}

#[test]
fn exported_provider_prefers_streaming_and_returns_terminal_message() {
    let mut api = api();
    api.register_provider = Some(unused_one_shot);
    api.register_streaming_provider = Some(register_stream);
    assert_eq!(rpi_plugin_register(&api), 0);
}

#[test]
fn exported_provider_falls_back_to_one_shot_without_declarations() {
    let mut api = api();
    api.declare = None;
    api.register_provider = Some(register_one_shot);
    assert_eq!(rpi_plugin_register(&api), 0);
}

#[test]
fn exported_provider_propagates_declaration_errors() {
    extern "C" fn refuse(_: sdk::StbStringRef) -> i32 {
        42
    }
    let mut api = api();
    api.declare = Some(refuse);
    api.register_provider = Some(unused_one_shot);
    assert_eq!(rpi_plugin_register(&api), 42);
}

#[test]
fn exported_provider_requires_a_host_registration_slot() {
    assert_ne!(rpi_plugin_register(&api()), 0);
}
