#![cfg(feature = "provider")]
use rpi_ai::{Api, AssistantMessage, Context, Model, Provider, SimpleStreamOptions};
use rpi_plugin_sdk as sdk;
use std::{cell::RefCell, ffi::c_void, ptr, sync::Arc};

fn factory(
    _: &sdk::provider::ProviderContext,
    _: &Model,
    options: &SimpleStreamOptions,
) -> Result<Arc<dyn Provider>, String> {
    if options.api_key.as_deref() == Some("fail") {
        return Err("client build failed".into());
    }
    Ok(rpi_ai::providers::faux::FauxProvider::new(
        rpi_ai::providers::faux::FauxScript::new()
            .with_text(options.api_key.as_deref().unwrap_or("anonymous")),
    ))
}
sdk::export_provider_with_context!(
    "context-test",
    "http://localhost",
    "faux",
    serde_json::json!({"id":"context-test"}),
    factory
);

type Hooks = (
    sdk::PluginBeginShutdownFn,
    sdk::PluginFinishShutdownFn,
    usize,
);
type Registered = (sdk::ProviderStreamRequestFn, sdk::FreeStringFn, usize);
thread_local! {
    static HOOKS: RefCell<Option<Hooks>> = const { RefCell::new(None) };
    static REGISTERED: RefCell<Option<Registered>> = const { RefCell::new(None) };
}
extern "C" fn lifecycle(
    begin: sdk::PluginBeginShutdownFn,
    finish: sdk::PluginFinishShutdownFn,
    data: *mut c_void,
) -> i32 {
    HOOKS.with(|slot| *slot.borrow_mut() = Some((begin, finish, data as usize)));
    0
}
extern "C" fn register(
    _: sdk::StbStringRef,
    _: sdk::StbStringRef,
    _: sdk::StbStringRef,
    request: sdk::ProviderStreamRequestFn,
    free: sdk::FreeStringFn,
    data: *mut c_void,
) -> i32 {
    REGISTERED.with(|slot| *slot.borrow_mut() = Some((request, free, data as usize)));
    0
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
        declare: None,
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

extern "C" fn emit(raw: sdk::StbString, data: *mut c_void) -> i32 {
    let _owned =
        unsafe { sdk::callbacks::OwnedString::from_raw(raw, Some(sdk::callbacks::free_string)) };
    assert_eq!(std::thread::current().id(), unsafe {
        *data.cast::<std::thread::ThreadId>()
    });
    0
}
fn call(registration: Registered, key: &str) -> AssistantMessage {
    let (request, free, data) = registration;
    let model = Model::new("m", "M", Api::Faux, "context-test", "http://localhost");
    let json =
        serde_json::json!({"model":model,"context":Context::new(vec![]),"options":{"apiKey":key}})
            .to_string();
    let mut out = sdk::StbString::empty();
    let mut thread_id = std::thread::current().id();
    assert_eq!(
        request(
            sdk::StbStringRef::from_str(&json),
            Some(emit),
            (&mut thread_id as *mut std::thread::ThreadId).cast(),
            ptr::null(),
            None,
            ptr::null_mut(),
            &mut out,
            data as *mut c_void
        ),
        0
    );
    let owned = unsafe { sdk::callbacks::OwnedString::from_raw(out, Some(free)) };
    serde_json::from_str(&owned.to_string_lossy()).unwrap()
}
#[test]
fn exported_context_handles_concurrent_credentials_errors_shutdown_and_old_hosts() {
    for has_lifecycle in [false, true] {
        let mut api = api();
        api.register_streaming_provider = Some(register);
        api.register_plugin_lifecycle = has_lifecycle.then_some(lifecycle);
        assert_eq!(rpi_plugin_register(&api), 0);
        let registration = REGISTERED.with(|slot| slot.borrow_mut().take().unwrap());
        assert_eq!(registration.2 != 0, has_lifecycle);
        let workers: Vec<_> = ["first-secret", "second-secret"].into_iter().map(|key| std::thread::spawn(move || {
            let message = call(registration, key);
            assert!(matches!(message.content.as_slice(), [rpi_ai::Content::Text(text)] if text.text == key));
        })).collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let failed = call(registration, "fail");
        assert_eq!(failed.stop_reason, rpi_ai::StopReason::Error);
        assert_eq!(failed.error_message.as_deref(), Some("client build failed"));
        if has_lifecycle {
            let (begin, finish, data) = HOOKS.with(|slot| slot.borrow_mut().take().unwrap());
            begin(data as *mut c_void);
            begin(data as *mut c_void);
            let rejected = call(registration, "after-shutdown");
            assert_eq!(rejected.stop_reason, rpi_ai::StopReason::Error);
            assert_eq!(finish(data as *mut c_void), 0);
        }
    }
}
