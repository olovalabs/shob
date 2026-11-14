//! Rust-native (cdylib) plugin loader + `AgentTool` adapter for rpi.
//!
//! This is Part B1 of the extension-alignment plan. `rpi` loads extensions as
//! **compiled Rust cdylibs** (`.dll`/`.so`/`.dylib`) via `libloading` — **not**
//! TS/jiti — because we control both sides and the plugin is Rust
//! (`动态加载可以加载rs 的代码 没必要是 ts`). The ABI contract lives in
//! [`rpi_plugin_sdk`]; this crate is the **host side** that loads plugins and
//! bridges their tools/events into rpi's native async types.
//!
//! ## Crate DAG position
//!
//! Depends on **`rpi-plugin-sdk` + `rpi-ai` + `rpi-agent` only** (NOT
//! `rpi-harness`). The harness consumes this crate's adapters as trait objects
//! via `AgentHarnessOptions` injection, so `rpi-harness` never imports
//! `rpi-extensions` — cycle-free (verified by adversarial review of the first
//! draft, which wrongly added a `rpi-harness` dep).
//!
//! ## The async-across-ABI bridge (load-bearing — corrected vs first draft)
//!
//! A plugin tool drives an execution through **four** plugin-exported fns
//! (`execute`→handle, `poll`, `cancel`, `destroy`) — see
//! [`rpi_plugin_sdk::ToolExecuteFn`] etc. The host's [`PluginToolAdapter`]
//! impls `AgentTool::execute` by:
//!
//! 1. **NOT owning a runtime.** Acquire the ambient runtime
//!    (`tokio::runtime::Handle::try_current()`) — the adapter only ever runs
//!    inside the agent loop's runtime.
//! 2. Set up an **unbounded mpsc** for `ToolResultPartial` (async side drains +
//!    forwards to `on_update`) and a **oneshot** for the terminal
//!    `AgentToolResult`.
//! 3. `handle.spawn_blocking(move || drive(plugin, cancel_flag, partial_tx,
//!    done_tx))` — the blocking driver loops `poll` until `Done`/`Err`,
//!    forwarding `Pending` partials through the mpsc (via a `catch_unwind`
//!    trampoline so a panicking partial callback can't unwind across FFI),
//!    then sends the terminal result + calls `destroy` **exactly once** on exit.
//! 4. The async future `select!`s between the oneshot (terminal) and
//!    `signal.cancelled()` (the child token). On cancel: **set an `AtomicBool`
//!    cancel flag (SeqCst)** so the blocking driver observes it; **keep
//!    awaiting the oneshot** (never drop the driver — `spawn_blocking` tasks
//!    run to completion regardless of outer-future drop, so dropping is a
//!    thread leak).
//! 5. `Drop`/drop-guard sets **only the cancel flag** — never calls `destroy`
//!    synchronously (the driver may be mid-`poll`); `destroy` is called exactly
//!    once by the blocking driver.
//!
//! `cancel` ≠ `destroy`: the first draft conflated them → UAF/double-free. Here
//! `cancel` is an idempotent thread-safe flag-set; `destroy` is the single
//! free, owned by the driver.
//!
//! Wake-capable tools use an owned async driver with serialized, short
//! `spawn_blocking` FFI calls. Wake notifications are retained across polls;
//! an idle `Pending` execution occupies no blocking worker.
//!
//! ## Events
//!
//! [`ExtensionEmitter`] impls `AgentEmitter` by subscribing to the host's
//! `broadcast::Sender<AgentEvent>`, translating each `AgentEvent` → a
//! [`rpi_plugin_sdk::StablePluginEvent`], and dispatching to every registered
//! handler for the event's tag — all dispatch wrapped in `catch_unwind`. The
//! 33-category `on()` surface (B3) is driven through this emitter; the
//! 10 already-emitted `AgentEvent` variants fold into the matching tags now,
//! and the remaining tags light up as B3/B4/B5 add the emission points.

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use rpi_plugin_sdk::{
    BeforeAgentStartFn, EventHandlerFn, EventTag, FreeStringFn, OAuthRequestControlFn,
    OAuthRequestFn, ProviderRequestFn, ProviderStreamRequestFn, RenderFn, ResourcesDiscoverFn,
    RuntimeActionFn, StablePluginEvent, StableToolSchema, StbString, StbStringRef, ToolCancelFn,
    ToolDestroyFn, ToolExecuteFn, ToolPollFn,
};
use thiserror::Error;

pub use actions::{
    reload_callback_from_mailbox, trampoline_runtime_action, ActionBridge, ReloadMailbox,
    ReloadModelsMailbox, ReloadModelsRequest, RuntimeActionHost, SideQuestionSink, UiDialogMailbox,
    UiDialogRequest,
};
pub use editor_text::{EditorTextEdit, EditorTextMailbox, EditorTextMode};
pub use event_log::{event_log_path, log_handler_invocation, EventLogger};
pub use loader::{
    load_dir, load_one, load_session, load_session_mixed, load_session_mixed_guarded,
    merge_registries, ExtensionSession, LoadedPlugin, PluginKeepalive, PluginLoadError,
};
pub use oauth::{
    list as list_oauth_providers, request as request_oauth,
    request_with_cancel as request_oauth_with_cancel, OAuthActionResponse, OAuthExtensionError,
    OAuthProviderInfo, OAuthResponseKind,
};
pub use prompt_transform::emit_before_agent_start;
pub use provider::PluggableProvider;
pub use provider_hooks::ExtensionProviderHooks;
pub use registry::{
    assert_active, current_platform, platform_allows, BeforeAgentStartHandler, ExtensionRegistry,
    ExtensionTool, ProviderManifest, RegisteredFlag, RegisteredHandler, RegisteredOAuthProvider,
    RegisteredProvider, RegisteredRenderer, RegisteredRendererKind, RegisteredShortcut,
    RegistryEntry, RegistrySnapshot, ResourcesDiscoverHandler, ToolPromptMetadata,
    DEFAULT_PRIORITY,
};
pub use resources::{emit_resources_discover, DiscoveredResources};
pub use status::ExtensionStatusMailbox;
pub use tool_result_transform::{emit_tool_result, with_tool_result_transform};
mod panel;
pub use panel::{ExtensionPanel, PanelAnchor, PanelHighlight, PanelHighlightColor, PanelLayout};
pub use tool::{PluginToolAdapter, PluginToolHandle, ToolCallContext};
pub use translate::{
    dispatch_data_event, dispatch_data_event_claiming, dispatch_empty_event,
    dispatch_lifecycle_event, dispatch_tool_call_event, ExtensionEmitter, TeeEmitter,
};

pub use stdio::StdioLifecycle;
mod actions;
mod editor_text;
mod event_log;
mod loader;
mod oauth;
mod prompt_transform;
mod provider;
mod provider_hooks;
mod registry;
mod resources;
mod status;
mod stdio;
mod tool;
#[cfg(test)]
mod tool_prompt_tests;
mod tool_result_transform;
mod translate;

/// A diagnostics/event sink the host wires so the loader + adapter can report
/// plugin-load skips, ABI mismatches, panics caught at the FFI boundary, etc.
/// Mirrors the `diagnostic` channel pi surfaces for extensions.
pub trait PluginDiagnostics: Send + Sync {
    /// A non-fatal warning (e.g. "skipped plugin foo: ABI version mismatch").
    fn warn(&self, message: &str);
    /// A plugin that registered something but a registration slot is not yet
    /// wired on the host (e.g. an event tag the host does not emit yet).
    fn unsupported(&self, message: &str);
}

/// A no-op diagnostics sink (the default when the host does not supply one).
#[derive(Default)]
pub struct NullDiagnostics;

impl PluginDiagnostics for NullDiagnostics {
    fn warn(&self, _message: &str) {}
    fn unsupported(&self, _message: &str) {}
}

/// Error returned by [`PluginToolAdapter::execute`] when the plugin side failed
/// (terminal `Err` from `poll`, or the drive handle was never produced).
#[derive(Debug, Error)]
pub enum PluginToolError {
    #[error("plugin execute returned a null handle (allocation failure)")]
    NullHandle,
    #[error("plugin error: {0}")]
    Plugin(String),
    #[error("runtime unavailable: {0}")]
    NoRuntime(String),
}

/// Producer-side deallocator for strings allocated by the host's SDK instance.
/// Plugin-produced strings must use the plugin's registered deallocator.
pub use rpi_plugin_sdk::callbacks::free_string as host_free_string;

// ===========================================================================
// HostApi — the PluginApi the host builds and hands to each plugin's register
// ===========================================================================

/// The host state a [`PluginApi`] closes over. Held behind `Arc` so the fn
/// pointers (which are `extern "C"`,不好做闭包) can recover the host state via
/// the `user_data` slot — but since `extern "C" fn` cannot capture, the host
/// stores per-registration receiver state in the [`HostApi`] itself keyed by
/// nothing (single registry per host), and the fns are thin trampolines that
/// read a process-global-attached registry. The host builds one `HostApi` per
/// load session; the `register_*` trampolines forward into it.
///
/// This struct is `Send + Sync` (registry is `Mutex`-guarded).
pub struct HostApi {
    lifecycle: Mutex<Vec<PluginLifecycle>>,
    shutting_down: std::sync::atomic::AtomicBool,
    registry: Mutex<Option<ExtensionRegistry>>,
    /// Display name of the plugin currently registering (from the cdylib file
    /// stem). Stamped onto every registration the plugin makes (e.g. event
    /// handlers) so host-side diagnostics can name the owning extension. Not
    /// part of the ABI — the plugin never sees it.
    name: String,
    /// B5a: the plugin→host action bridge, carried in
    /// [`PluginApi::user_data`] so [`trampoline_runtime_action`] can recover
    /// the harness state from ANY thread a plugin calls from (post-register, no
    /// thread-local). `None` keeps the no-bridge stub `runtime_action` and the
    /// register `user_data` (the `HostApi` pointer) — so call sites that
    /// don't pass a bridge behave exactly as before.
    action_bridge: Option<Arc<ActionBridge>>,
}

impl HostApi {
    /// Build a fresh host API bound to the given registry + diagnostics. With
    /// no action bridge, `runtime_action` stays the stub returning `-1`. `name`
    /// is the registering plugin's display name (from the cdylib file stem).
    pub fn new(
        name: impl Into<String>,
        registry: ExtensionRegistry,
        _diagnostics: Arc<dyn PluginDiagnostics>,
    ) -> Arc<Self> {
        Arc::new(Self {
            lifecycle: Mutex::new(Vec::new()),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            registry: Mutex::new(Some(registry)),
            name: name.into(),
            action_bridge: None,
        })
    }

    pub(crate) fn begin_shutdown(&self) {
        if !self
            .shutting_down
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            let lifecycle = self.lifecycle.lock().unwrap().clone();
            for lifecycle in lifecycle {
                (lifecycle.begin)(lifecycle.user_data);
            }
        }
    }

    pub(crate) fn finish_shutdown(&self) -> bool {
        let lifecycle = self.lifecycle.lock().unwrap().clone();
        let mut success = true;
        for lifecycle in lifecycle {
            success &= (lifecycle.finish)(lifecycle.user_data) == 0;
        }
        success
    }

    /// The registering plugin's display name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// B5a: build a host API that wires the real [`trampoline_runtime_action`]
    /// via the bridge. The bridge is cloned into every plugin's vtable
    /// `user_data` so post-register `runtime_action` calls recover it on any
    /// thread. `rpi-cli` keeps one master `Arc<ActionBridge>` per session; the
    /// plugin gets a scoped context sharing the session's facilities. The
    /// loader retains that context with the library, so its pointer stays valid
    /// after registration. `name` is the registering plugin's display name.
    pub fn with_action_bridge(
        name: impl Into<String>,
        registry: ExtensionRegistry,
        _diagnostics: Arc<dyn PluginDiagnostics>,
        action_bridge: Arc<ActionBridge>,
    ) -> Arc<Self> {
        let name = name.into();
        let action_bridge = action_bridge.for_plugin(&name);
        Arc::new(Self {
            lifecycle: Mutex::new(Vec::new()),
            shutting_down: std::sync::atomic::AtomicBool::new(false),
            registry: Mutex::new(Some(registry)),
            name,
            action_bridge: Some(action_bridge),
        })
    }

    fn with_registry<R>(&self, f: impl FnOnce(&mut ExtensionRegistry) -> R) -> Option<R> {
        let mut guard = self.registry.lock().expect("host api registry lock");
        guard.as_mut().map(f)
    }

    /// Detach the registry (call after registration completes so the host can
    /// move it out for use). The `Mutex<Option<...>>` is left `None`.
    pub fn take_registry(&self) -> Option<ExtensionRegistry> {
        self.registry.lock().expect("host api registry lock").take()
    }

    /// Build the [`PluginApi`] struct passed to `rpi_plugin_register`.
    ///
    /// Every optional registrar slot resolves to a real `extern "C"` trampoline
    /// that forwards into `self`'s registry (so a plugin that calls
    /// `register_tool` / `register_command` / renderer registration sees its
    /// registration land). `runtime_action` resolves to the real
    /// [`trampoline_runtime_action`] when a bridge is present (B5a), else the
    /// stub returning `-1`.
    ///
    /// **`user_data`**: the register trampolines recover host state via the
    /// thread-local `CURRENT_HOST_API` (set for the duration of register in
    /// `load_one`) — they ignore `user_data`. Post-register `runtime_action`
    /// cannot use the thread-local (plugin calls from foreign threads), so when
    /// a bridge is present `user_data` carries an opaque registry handle, never
    /// its address. Dispatch upgrades a weak reference before accessing host
    /// state, so an expired candidate reload fails without dereferencing it.
    pub fn build_vtable(self: &Arc<Self>) -> rpi_plugin_sdk::PluginApi {
        // When a bridge is present, user_data carries it (post-register action
        // recovery). Otherwise keep the register-path HostApi pointer (harmless
        // — register trampolines use the thread-local and ignore user_data).
        // Cast both arms to the `RuntimeActionFn` pointer type — distinct fn
        // items have unique types even with identical signatures, so the match
        // needs a common fn-pointer type to unify on (the struct field is
        // `RuntimeActionFn`, a bare `extern "C" fn` alias, not an `Option`).
        let (runtime_action_fn, ud) = match &self.action_bridge {
            Some(bridge) => (
                trampoline_runtime_action as RuntimeActionFn,
                bridge.user_data(),
            ),
            None => (
                stub_runtime_action as RuntimeActionFn,
                Arc::as_ptr(self) as *mut c_void,
            ),
        };
        rpi_plugin_sdk::PluginApi {
            abi_version: rpi_plugin_sdk::RPI_PLUGIN_ABI_VERSION_UNIFIED,
            struct_size: std::mem::size_of::<rpi_plugin_sdk::PluginApi>() as u32,
            free_string: host_free_string,
            register_tool: Some(trampoline_register_tool),
            register_tool_with_wake: Some(trampoline_register_tool_with_wake),
            register_plugin_lifecycle: Some(trampoline_register_plugin_lifecycle),
            register_tool_prompt: Some(trampoline_register_tool_prompt),
            register_command: Some(trampoline_register_command),
            register_shortcut: Some(trampoline_register_shortcut),
            register_flag: Some(trampoline_register_flag),
            register_provider: Some(trampoline_register_provider), // B5c
            register_streaming_provider: Some(trampoline_register_streaming_provider), // B5c-s
            register_message_renderer: Some(trampoline_register_message_renderer), // B5c
            register_markdown_transformer: Some(trampoline_register_markdown_transformer), // B5c
            register_entry_renderer: Some(trampoline_register_entry_renderer), // B5c
            register_event_handler: Some(trampoline_register_event_handler),
            register_resources_discover: Some(trampoline_register_resources_discover),
            // Appended last so `struct_size` grows without shifting any earlier
            // offset (a plugin built against the previous layout keeps working).
            register_before_agent_start: Some(trampoline_register_before_agent_start),
            register_oauth_provider: Some(trampoline_register_oauth_provider),
            register_oauth_provider_cancellable: Some(
                trampoline_register_oauth_provider_cancellable,
            ),
            register_tool_result: Some(trampoline_register_tool_result),
            runtime_action: runtime_action_fn,
            dispatch_event: Some(trampoline_dispatch_event),
            user_data: ud,
            declare: Some(trampoline_declare),
        }
    }
}

#[derive(Clone, Copy)]
struct PluginLifecycle {
    begin: rpi_plugin_sdk::PluginBeginShutdownFn,
    finish: rpi_plugin_sdk::PluginFinishShutdownFn,
    user_data: *mut c_void,
}

// The plugin contract permits lifecycle callbacks on host cleanup threads.
unsafe impl Send for PluginLifecycle {}
unsafe impl Sync for PluginLifecycle {}

extern "C" fn trampoline_register_plugin_lifecycle(
    begin: rpi_plugin_sdk::PluginBeginShutdownFn,
    finish: rpi_plugin_sdk::PluginFinishShutdownFn,
    user_data: *mut c_void,
) -> i32 {
    with_current_api(|api| {
        if api.shutting_down.load(std::sync::atomic::Ordering::Acquire) {
            return 1;
        }
        let mut lifecycle = api.lifecycle.lock().unwrap();
        lifecycle.push(PluginLifecycle {
            begin,
            finish,
            user_data,
        });
        0
    })
    .unwrap_or(1)
}

// Thread-local "current host api" for the duration of a plugin register call.
// Owned for the duration of registration. Nested registrations restore their
// caller's context, including when a Rust callback unwinds.
thread_local! {
    static CURRENT_HOST_API: std::cell::RefCell<Option<Arc<HostApi>>> = const { std::cell::RefCell::new(None) };
}

struct CurrentApiGuard {
    previous: Option<Arc<HostApi>>,
    // Restoration must run on the same thread that entered registration.
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Drop for CurrentApiGuard {
    fn drop(&mut self) {
        let displaced = CURRENT_HOST_API.with(|slot| slot.replace(self.previous.take()));
        drop(displaced);
    }
}

fn enter_current_api(api: &Arc<HostApi>) -> CurrentApiGuard {
    let previous = CURRENT_HOST_API.with(|slot| slot.replace(Some(Arc::clone(api))));
    CurrentApiGuard {
        previous,
        _thread_bound: std::marker::PhantomData,
    }
}

/// Run `f` with an owned snapshot of the current host api. Release the
/// RefCell borrow before calling user code so registration can reenter.
fn with_current_api<R>(f: impl FnOnce(&HostApi) -> R) -> Option<R> {
    let api = CURRENT_HOST_API.with(|slot| slot.borrow().clone())?;
    Some(f(&api))
}

/// True if a current host api is set (cheap check used by trampolines that only
/// need presence, not the api itself).
fn current_api_present() -> bool {
    CURRENT_HOST_API.with(|slot| slot.borrow().is_some())
}

// --- the registrar trampolines (extern "C", forward into the current api) ---

extern "C" fn trampoline_register_tool(
    schema: *const StableToolSchema,
    execute_fn: ToolExecuteFn,
    poll_fn: ToolPollFn,
    cancel_fn: ToolCancelFn,
    destroy_fn: ToolDestroyFn,
    plugin_free_string: FreeStringFn,
) -> i32 {
    register_tool(
        schema,
        execute_fn,
        poll_fn,
        cancel_fn,
        destroy_fn,
        plugin_free_string,
        None,
    )
}

extern "C" fn trampoline_register_tool_with_wake(
    schema: *const StableToolSchema,
    execute_fn: ToolExecuteFn,
    poll_fn: ToolPollFn,
    cancel_fn: ToolCancelFn,
    destroy_fn: ToolDestroyFn,
    plugin_free_string: FreeStringFn,
    set_waker_fn: rpi_plugin_sdk::ToolSetWakerFn,
) -> i32 {
    register_tool(
        schema,
        execute_fn,
        poll_fn,
        cancel_fn,
        destroy_fn,
        plugin_free_string,
        Some(set_waker_fn),
    )
}

fn register_tool(
    schema: *const StableToolSchema,
    execute_fn: ToolExecuteFn,
    poll_fn: ToolPollFn,
    cancel_fn: ToolCancelFn,
    destroy_fn: ToolDestroyFn,
    plugin_free_string: FreeStringFn,
    set_waker_fn: Option<rpi_plugin_sdk::ToolSetWakerFn>,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    if schema.is_null() {
        return 1;
    }
    // SAFETY: the plugin guarantees `schema` is valid for the call; we copy the
    // strings out immediately (under the borrow) and free them via the plugin's
    // `plugin_free_string` right after, so no retention past the call.
    let (name, description, parameters_value) = unsafe {
        let s = &*schema;
        let name =
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(s.name, Some(plugin_free_string));
        let description = rpi_plugin_sdk::callbacks::OwnedString::from_raw(
            s.description,
            Some(plugin_free_string),
        );
        let parameters = rpi_plugin_sdk::callbacks::OwnedString::from_raw(
            s.parameters,
            Some(plugin_free_string),
        );
        (
            name.to_string_lossy(),
            description.to_string_lossy(),
            serde_json::from_str::<serde_json::Value>(&parameters.to_string_lossy()).ok(),
        )
    };
    let Some(parameters_value) = parameters_value else {
        return 2;
    };
    if name.trim().is_empty() || !parameters_value.is_object() {
        return 2;
    }
    let tool = rpi_ai::types::Tool {
        name,
        description,
        parameters: rpi_ai::types::Schema::new(parameters_value),
        constrained_sampling: None,
    };
    let handle = PluginToolHandle {
        execute_fn,
        poll_fn,
        cancel_fn,
        destroy_fn,
        plugin_free_string,
        set_waker_fn,
        runtime_handle: None,
    };
    // `register_tool` returns `bool` (true on overwrite of a prior same-named
    // tool). For the plugin a name collision is not a hard failure — always
    // return 0 (success) when the registry accepted it; -1 only if the registry
    // was already detached.
    let ok = with_current_api(|api| {
        let mut handle = handle;
        handle.runtime_handle = api
            .action_bridge
            .as_ref()
            .map(|bridge| bridge.user_data() as usize);
        api.with_registry(|reg| reg.register_tool(tool, handle))
            .is_some()
    });
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

extern "C" fn trampoline_register_tool_prompt(
    name: StbStringRef,
    metadata_json: StbStringRef,
) -> i32 {
    rpi_plugin_sdk::guard_or(-1, || {
        with_current_api(|api| {
            // SAFETY: plugin-supplied refs are borrowed for this registration call.
            let name = unsafe { name.as_str() };
            let json = unsafe { metadata_json.as_str() };
            let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
                return 2;
            };
            if !value.is_object() {
                return 2;
            }
            let Ok(prompt) = serde_json::from_value::<ToolPromptMetadata>(value) else {
                return 2;
            };
            match api.with_registry(|reg| reg.register_tool_prompt(name, prompt)) {
                Some(true) => 0,
                Some(false) => 1,
                None => -1,
            }
        })
        .unwrap_or(-1)
    })
}

extern "C" fn trampoline_register_command(
    name: StbStringRef,
    description: StbStringRef,
    handler: rpi_plugin_sdk::CommandHandlerFn,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    // SAFETY: the plugin guarantees the refs are valid for this call.
    let (name, description) =
        unsafe { (name.as_str().to_string(), description.as_str().to_string()) };
    let ok = with_current_api(|api| {
        // Commands carry the same opaque owner binding as PluginApi. Never
        // make a plugin cache a registration pointer globally: one DLL can be
        // loaded for multiple sessions or a rolled-back reload candidate.
        let user_data = api
            .action_bridge
            .as_ref()
            .map_or(std::ptr::null_mut(), |bridge| bridge.user_data());
        match api.with_registry(|reg| reg.register_command(name, description, handler, user_data)) {
            Some(_) => true,
            None => false,
        }
    });
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

extern "C" fn trampoline_register_shortcut(key: StbStringRef, description: StbStringRef) -> i32 {
    if !current_api_present() {
        return -1;
    }
    // SAFETY: the plugin guarantees these borrowed refs are valid for the
    // duration of the registration call; copy before returning.
    let (key, description) =
        unsafe { (key.as_str().to_string(), description.as_str().to_string()) };
    // Record the claim. The key is NOT bound to a callback here: the owning
    // plugin receives presses/releases through its `Input` event handler
    // (dispatched by the TUI) and decides what to do. The host only needs to
    // know the key is claimed so it can route it away from the editor.
    let ok = with_current_api(|api| {
        match api
            .with_registry(|reg| reg.register_shortcut(api.name().to_string(), key, description))
        {
            Some(_) => true,
            None => false,
        }
    });
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

extern "C" fn trampoline_register_flag(name: StbStringRef, description: StbStringRef) -> i32 {
    if !current_api_present() {
        return -1;
    }
    // SAFETY: the plugin guarantees these borrowed refs are valid for the
    // duration of the registration call; copy them before returning.
    let (name, description) =
        unsafe { (name.as_str().to_string(), description.as_str().to_string()) };
    if name.trim().is_empty() || name.starts_with('-') || name.contains('=') || name.contains(' ') {
        return 2;
    }
    let ok = with_current_api(|api| {
        match api.with_registry(|reg| reg.register_flag(name, description)) {
            Some(_) => true,
            None => false,
        }
    });
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

extern "C" fn trampoline_register_event_handler(
    tag: EventTag,
    handler: EventHandlerFn,
    user_data: *mut c_void,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    let ok = with_current_api(|api| {
        match api.with_registry(|reg| {
            reg.register_event_handler(api.name().to_string(), tag, handler, user_data)
        }) {
            Some(_) => true,
            None => false,
        }
    });
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

/// Plugin `declare` trampoline. Runs synchronously during a plugin's
/// `register` (thread-local `CURRENT_HOST_API` is set). The plugin hands a
/// borrowed JSON payload (`{"priority":60,"platforms":["linux"]}`); we parse
/// and apply it to this plugin's registry (retro-updating any handlers already
/// registered, so declare order doesn't matter). Returns `0` on a parsed
/// payload, `1` on malformed JSON, `-1` outside register.
extern "C" fn trampoline_declare(json: StbStringRef) -> i32 {
    if !current_api_present() {
        return -1;
    }
    // SAFETY: the plugin guarantees `json` is valid UTF-8 for the call; we read
    // it immediately (copy into the registry) and never retain the borrow.
    let text = unsafe { json.as_str() };
    match with_current_api(|api| {
        api.with_registry(|reg| {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
                return false;
            };
            if value.get("ownsStdio").and_then(serde_json::Value::as_bool) == Some(true) {
                if let Some(bridge) = &api.action_bridge {
                    if let Err(error) = bridge.claim_stdio() {
                        tracing::error!("{error}");
                        return false;
                    }
                } else {
                    return false; // A stdio owner requires a live lifecycle bridge.
                }
            }
            reg.apply_declaration_json(text)
        })
    }) {
        Some(Some(true)) => 0,
        Some(Some(false)) => 1,
        _ => -1,
    }
}

/// B5c: `register_provider` trampoline. Runs synchronously inside a plugin's
/// `register` call (thread-local `CURRENT_HOST_API` is set). The plugin hands its
/// `provider_id`/`base_url`/`api_style` (borrowed `StbStringRef`s), its
/// `request_fn`, its own `plugin_free_string` (the `out` StbString `request_fn`
/// later produces is plugin-owned — the host reclaims it via this fn), and its
/// opaque `user_data`. We copy the id/base_url/api_style to owned `String`s
/// (they're the provider's identity, read when building the
/// [`PluggableProvider`](crate::PluggableProvider) — we can't keep the borrowed
/// refs past register), then store the full record in the registry.
extern "C" fn trampoline_register_provider(
    provider_id: StbStringRef,
    base_url: StbStringRef,
    api_style: StbStringRef,
    request_fn: ProviderRequestFn,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    // SAFETY: the plugin guarantees the refs are valid for this call.
    let record = crate::registry::RegisteredProvider {
        provider_id: unsafe { provider_id.as_str().to_string() },
        base_url: unsafe { base_url.as_str().to_string() },
        api_style: unsafe { api_style.as_str().to_string() },
        request_fn,
        // One-shot registration: no streaming fn. A plugin that also calls
        // `register_streaming_provider` upgrades this record in place.
        stream_fn: None,
        plugin_free_string,
        user_data,
    };
    let ok = with_current_api(
        |api| match api.with_registry(|reg| reg.register_provider(record)) {
            Some(_) => true,
            None => false,
        },
    );
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

/// B5c-s: `register_streaming_provider` trampoline. Identical bookkeeping to
/// [`trampoline_register_provider`], but records the plugin's
/// [`ProviderStreamRequestFn`] as the record's `stream_fn` — the host then drives
/// the per-chunk callback path (with the cancellation control surface) instead of
/// the one-shot `request_fn`.
///
/// Upgrading an id that a plugin already registered one-shot **replaces** the
/// record in place (see `ExtensionRegistry::register_streaming_provider`), so a
/// plugin that registers both ends up on the streaming path.
extern "C" fn trampoline_register_streaming_provider(
    provider_id: StbStringRef,
    base_url: StbStringRef,
    api_style: StbStringRef,
    request_fn: ProviderStreamRequestFn,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    // The one-shot slot stays non-null (a placeholder that fails loudly) so the
    // record is uniform: the host prefers `stream_fn` whenever it is present.
    let record = crate::registry::RegisteredProvider {
        provider_id: unsafe { provider_id.as_str().to_string() },
        base_url: unsafe { base_url.as_str().to_string() },
        api_style: unsafe { api_style.as_str().to_string() },
        request_fn: unsupported_one_shot_provider_request,
        stream_fn: Some(request_fn),
        plugin_free_string,
        user_data,
    };
    let ok = with_current_api(|api| {
        api.with_registry(|reg| reg.register_streaming_provider(record))
            .is_some()
    });
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

/// Placeholder `request_fn` for a streaming-only registration. The host never
/// calls it (`RegisteredProvider::stream_fn.is_some()` routes to the streaming
/// driver), so this exists purely to keep the record's one-shot field non-null
/// and to fail loudly with a diagnostic instead of a null deref if a future code
/// path ever does call it.
extern "C" fn unsupported_one_shot_provider_request(
    _req_json: StbStringRef,
    _out: *mut StbString,
    _user_data: *mut c_void,
) -> i32 {
    tracing::error!(
        "a streaming-only plugin provider was driven through the one-shot request path"
    );
    1
}

/// Register an OAuth provider manifest and action callback. The manifest is
/// copied immediately; token-bearing responses remain plugin-owned and are
/// reclaimed through the plugin free function when invoked by the host OAuth
/// bridge. The legacy callback cannot observe cancellation.
extern "C" fn trampoline_register_oauth_provider(
    manifest_json: StbStringRef,
    request_fn: OAuthRequestFn,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    register_oauth_provider_record(
        manifest_json,
        request_fn,
        None,
        plugin_free_string,
        user_data,
    )
}

/// Register an OAuth provider whose action callback takes an
/// [`rpi_plugin_sdk::OAuthRequestControl`]. The host passes a control that
/// reflects cancellation of each action. Otherwise identical to
/// [`trampoline_register_oauth_provider`].
extern "C" fn trampoline_register_oauth_provider_cancellable(
    manifest_json: StbStringRef,
    request_fn: OAuthRequestControlFn,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    register_oauth_provider_record(
        manifest_json,
        unsupported_legacy_oauth_request,
        Some(request_fn),
        plugin_free_string,
        user_data,
    )
}

/// Placeholder legacy callback for a cancellable registration. The host always
/// prefers `control_fn` when it is present, so this fails loudly if a future code
/// path ever calls it, rather than dereferencing a null pointer.
extern "C" fn unsupported_legacy_oauth_request(
    _req_json: StbStringRef,
    _out: *mut StbString,
    _user_data: *mut c_void,
) -> i32 {
    tracing::error!("a cancellable OAuth provider was driven through the legacy request path");
    1
}

fn register_oauth_provider_record(
    manifest_json: StbStringRef,
    request_fn: OAuthRequestFn,
    control_fn: Option<OAuthRequestControlFn>,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    let manifest = unsafe { manifest_json.as_str().to_string() };
    let record = crate::registry::RegisteredOAuthProvider {
        plugin: with_current_api(|api| api.name.clone()).unwrap_or_default(),
        manifest_json: manifest,
        request_fn,
        control_fn,
        plugin_free_string,
        user_data,
    };
    match with_current_api(|api| api.with_registry(|reg| reg.register_oauth_provider(record))) {
        Some(Some(true)) => 1,
        Some(Some(false)) => 0,
        _ => -1,
    }
}

/// B5c: the three renderer registrars share a single recording helper; each
/// trampoline below fixes its [`RegisteredRendererKind`] and forwards here.
fn register_renderer_common(
    name: StbStringRef,
    kind: crate::registry::RegisteredRendererKind,
    render_fn: RenderFn,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    // SAFETY: the plugin guarantees the ref is valid for this call.
    let record = crate::registry::RegisteredRenderer {
        name: unsafe { name.as_str().to_string() },
        kind,
        render_fn,
        plugin_free_string,
        user_data,
    };
    let ok = with_current_api(
        |api| match api.with_registry(|reg| reg.register_renderer(record)) {
            Some(_) => true,
            None => false,
        },
    );
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

extern "C" fn trampoline_register_message_renderer(
    name: StbStringRef,
    render_fn: RenderFn,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    register_renderer_common(
        name,
        crate::registry::RegisteredRendererKind::Message,
        render_fn,
        plugin_free_string,
        user_data,
    )
}

extern "C" fn trampoline_register_markdown_transformer(
    name: StbStringRef,
    render_fn: RenderFn,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    register_renderer_common(
        name,
        crate::registry::RegisteredRendererKind::Markdown,
        render_fn,
        plugin_free_string,
        user_data,
    )
}

extern "C" fn trampoline_register_entry_renderer(
    name: StbStringRef,
    render_fn: RenderFn,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    register_renderer_common(
        name,
        crate::registry::RegisteredRendererKind::Entry,
        render_fn,
        plugin_free_string,
        user_data,
    )
}

extern "C" fn trampoline_dispatch_event(_event: StablePluginEvent, _user_data: *mut c_void) -> i32 {
    // v1: a plugin emitting an event upstream — we accept it (return 0) but do
    // not yet forward to host subscribers (no upstream channel wired). B5 wires
    // the reverse-direction event bus.
    0
}

/// B5b: `register_resources_discover` trampoline. Runs synchronously inside a
/// plugin's `register` call (so the thread-local `CURRENT_HOST_API` is set → the
/// register-path recovery works, same as the other `trampoline_register_*` fns).
/// The plugin hands its `handler`, its own `plugin_free_string` (the `out`
/// StbString the handler later produces is plugin-owned — the host must reclaim
/// it via this fn), and its opaque `user_data`. The host stores all three in the
/// registry and later fans the discovery event out via `emit_resources_discover`.
extern "C" fn trampoline_register_resources_discover(
    handler: ResourcesDiscoverFn,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    let ok = with_current_api(|api| {
        match api.with_registry(|reg| {
            reg.register_resources_discover(handler, plugin_free_string, user_data)
        }) {
            Some(_) => true,
            None => false,
        }
    });
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

/// `register_tool_result` trampoline. Same shape as
/// [`trampoline_register_before_agent_start`].
extern "C" fn trampoline_register_tool_result(
    handler: rpi_plugin_sdk::ToolResultFn,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    let ok = with_current_api(|api| {
        api.with_registry(|reg| reg.register_tool_result(handler, plugin_free_string, user_data))
            .is_some()
    });
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

/// `register_before_agent_start` trampoline. Same shape as
/// [`trampoline_register_resources_discover`]: runs synchronously inside a
/// plugin's `register` call (thread-local `CURRENT_HOST_API` is set), stores the
/// handler plus the plugin's own `plugin_free_string` (the `out` StbString the
/// handler later produces is plugin-owned) and its opaque `user_data`.
extern "C" fn trampoline_register_before_agent_start(
    handler: BeforeAgentStartFn,
    plugin_free_string: FreeStringFn,
    user_data: *mut c_void,
) -> i32 {
    if !current_api_present() {
        return -1;
    }
    let ok = with_current_api(|api| {
        match api.with_registry(|reg| {
            reg.register_before_agent_start(handler, plugin_free_string, user_data)
        }) {
            Some(_) => true,
            None => false,
        }
    });
    if ok == Some(true) {
        0
    } else {
        -1
    }
}

/// Fallback for [`HostApi`]s built without an [`ActionBridge`] (the
/// `HostApi::new` path). Return -1 (`EPERM`-ish) so a plugin can detect
/// "unsupported" without crashing. When a bridge is present, `build_vtable`
/// installs [`trampoline_runtime_action`] instead.
extern "C" fn stub_runtime_action(
    _action_id: u32,
    _args: StbStringRef,
    _out: *mut StbString,
    _user_data: *mut c_void,
) -> i32 {
    -1
}

// Keep the `RuntimeActionFn` type name referenced for doc clarity / future wiring.
#[allow(dead_code)]
type _RuntimeActionFnDoc = RuntimeActionFn;

#[cfg(test)]
mod current_api_tests {
    use super::*;

    fn api(name: &str) -> Arc<HostApi> {
        HostApi::new(name, ExtensionRegistry::new(), Arc::new(NullDiagnostics))
    }

    #[test]
    fn all_lifecycle_contexts_finish_even_when_one_reports_failure() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Calls {
            begins: AtomicUsize,
            finishes: AtomicUsize,
        }
        extern "C" fn begin(data: *mut c_void) {
            unsafe { &*data.cast::<Calls>() }
                .begins
                .fetch_add(1, Ordering::SeqCst);
        }
        extern "C" fn finish(data: *mut c_void) -> i32 {
            unsafe { &*data.cast::<Calls>() }
                .finishes
                .fetch_add(1, Ordering::SeqCst);
            1
        }
        let calls = Calls {
            begins: AtomicUsize::new(0),
            finishes: AtomicUsize::new(0),
        };
        let owner = api("multiple-contexts");
        let registration = enter_current_api(&owner);
        for _ in 0..2 {
            assert_eq!(
                trampoline_register_plugin_lifecycle(
                    begin,
                    finish,
                    (&calls as *const Calls).cast_mut().cast()
                ),
                0
            );
        }
        drop(registration);
        owner.begin_shutdown();
        owner.begin_shutdown();
        assert_eq!(calls.begins.load(Ordering::SeqCst), 2);
        assert!(!owner.finish_shutdown());
        assert_eq!(calls.finishes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn registration_owns_its_api_and_releases_it_afterward() {
        let owner = api("owned");
        let weak = Arc::downgrade(&owner);
        let registration = enter_current_api(&owner);
        drop(owner);
        assert!(weak.upgrade().is_some());
        assert_eq!(
            with_current_api(|api| api.name().to_string()).as_deref(),
            Some("owned")
        );
        drop(registration);
        assert!(!current_api_present());
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn nested_registration_can_reenter_and_restores_the_outer_api() {
        let outer = api("outer");
        let outer_registration = enter_current_api(&outer);
        with_current_api(|current| {
            assert_eq!(current.name(), "outer");
            let inner = api("inner");
            let _inner_registration = enter_current_api(&inner);
            assert_eq!(
                with_current_api(|api| api.name().to_string()).as_deref(),
                Some("inner")
            );
        })
        .unwrap();
        assert_eq!(
            with_current_api(|api| api.name().to_string()).as_deref(),
            Some("outer")
        );
        drop(outer_registration);
        assert!(!current_api_present());
    }

    #[test]
    fn a_panicking_registration_restores_the_outer_api() {
        let outer = api("outer");
        let _outer_registration = enter_current_api(&outer);
        let inner = api("inner");
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _inner_registration = enter_current_api(&inner);
            assert_eq!(
                with_current_api(|api| api.name().to_string()).as_deref(),
                Some("inner")
            );
            panic!("registration failed");
        }));
        assert!(failed.is_err());
        assert_eq!(
            with_current_api(|api| api.name().to_string()).as_deref(),
            Some("outer")
        );
    }

    #[test]
    fn registration_context_is_thread_local() {
        let parent = api("parent");
        let _registration = enter_current_api(&parent);
        std::thread::spawn(|| {
            assert!(!current_api_present());
            let child = api("child");
            let _registration = enter_current_api(&child);
            assert_eq!(
                with_current_api(|api| api.name().to_string()).as_deref(),
                Some("child")
            );
        })
        .join()
        .unwrap();
        assert_eq!(
            with_current_api(|api| api.name().to_string()).as_deref(),
            Some("parent")
        );
    }
}
