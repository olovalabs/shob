//! The extension registry — accumulates registrations during a plugin's
//! `rpi_plugin_register` call, then snapshots for the host session.
//!
//! `HostApi` (in [`crate`]) holds an `ExtensionRegistry` behind a `Mutex` while
//! the plugin's register trampolines run; after register completes the host
//! [`take`]s it and builds a [`RegistrySnapshot`] the session keeps for its
//! lifetime. Staleness across a session swap / `/reload` is guarded by a shared
//! `Arc<AtomicBool>` "active" flag ([`assert_active`]).
//!
//! [`take`]: crate::HostApi::take_registry

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rpi_agent::error::AgentError;
use rpi_ai::types::Tool;
use rpi_plugin_sdk::{
    BeforeAgentStartFn, CommandHandlerFn, EventHandlerFn, EventTag, FreeStringFn,
    OAuthRequestControlFn, OAuthRequestFn, ProviderRequestFn, ProviderStreamRequestFn, RenderFn,
    ResourcesDiscoverFn, ToolResultFn, EVENT_TAG_COUNT,
};

use crate::tool::PluginToolHandle;

/// A provider package's catalog and authentication policy, supplied through
/// `declare({"providers":[...]})`. Credentials are always resolved by the host.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderManifest {
    pub id: String,
    pub models: Vec<rpi_ai::Model>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default = "requires_auth_default")]
    pub requires_auth: bool,
}

fn requires_auth_default() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Registered-item records
// ---------------------------------------------------------------------------

/// Explicit system-prompt contributions, separate from the provider-facing
/// schema. No snippet means the tool remains callable but is not listed.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolPromptMetadata {
    pub prompt_snippet: Option<String>,
    pub prompt_guidelines: Vec<String>,
}

impl ToolPromptMetadata {
    /// Match pi's whitespace handling; cross-tool deduplication happens when
    /// the CLI renders the active tool set.
    pub fn normalized(self) -> Self {
        let mut seen = std::collections::HashSet::new();
        Self {
            prompt_snippet: self
                .prompt_snippet
                .map(|snippet| snippet.split_whitespace().collect::<Vec<_>>().join(" "))
                .filter(|snippet| !snippet.is_empty()),
            prompt_guidelines: self
                .prompt_guidelines
                .into_iter()
                .map(|rule| rule.trim().to_owned())
                .filter(|rule| !rule.is_empty() && seen.insert(rule.clone()))
                .collect(),
        }
    }
}

/// A tool an extension registered: the provider-facing [`Tool`] schema + the
/// plugin's 4-function handle ([`PluginToolHandle`]) the host drives via
/// [`PluginToolAdapter`](crate::PluginToolAdapter).
///
/// `tool` is public so the host can merge schemas; `handle` is crate-private
/// (only [`PluginToolAdapter`](crate::PluginToolAdapter) drives it). `Clone`
/// derives because [`Tool`] is `Clone` and [`PluginToolHandle`] is `Copy`.
#[derive(Clone)]
pub struct ExtensionTool {
    /// The provider-facing tool definition (name/description/parameters).
    pub tool: Tool,
    /// Optional prompt contributions owned by the same registration as the tool.
    pub prompt: ToolPromptMetadata,
    pub(crate) handle: PluginToolHandle,
}

impl ExtensionTool {
    pub fn new(tool: Tool, handle: PluginToolHandle) -> Self {
        Self {
            tool,
            prompt: ToolPromptMetadata::default(),
            handle,
        }
    }

    /// The plugin-side function handles backing this tool. `Copy` (fn pointers
    /// + a `FreeStringFn`), so handing it out is free. Public so the host
    /// (e.g. rpi-cli's session builder) can wrap a registered tool in a
    /// [`PluginToolAdapter`](crate::PluginToolAdapter) outside this crate.
    pub fn handle(&self) -> PluginToolHandle {
        self.handle
    }
}

/// A registered slash command and its plugin callback.
/// The host supplies an opaque owner-scoped runtime handle, not plugin memory.
/// The plugin makes its callback safe to invoke from any host thread.
#[derive(Clone)]
pub struct RegisteredCommand {
    pub name: String,
    pub description: String,
    pub handler: CommandHandlerFn,
    pub user_data: *mut std::ffi::c_void,
}

/// A keyboard shortcut declared by a native extension (e.g. `"space"` for
/// push-to-talk). The host records it so the TUI can route keys to it; the
/// owning plugin receives the key via its `Input` event handler.
///
/// `key` is a normalized key name (`"space"`, `"enter"`, `"c"`, `"f1"`)
/// matching the editor's keybinding vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredShortcut {
    pub key: String,
    pub description: String,
    /// The owning extension's display name (host-side only, for diagnostics).
    pub plugin: String,
}

// SAFETY: user_data is an opaque host registry handle (never dereferenced),
// and plugin callbacks support invocation from any host thread.
unsafe impl Send for RegisteredCommand {}
unsafe impl Sync for RegisteredCommand {}

/// A CLI flag declared by a native extension. Values are supplied by the
/// host's parsed `Args::unknown_flags` map and read by the plugin through the
/// `RuntimeActionId::GetCliFlag` action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredFlag {
    pub name: String,
    pub description: String,
}

/// A registered `on(tag)` event handler. `user_data` is the plugin's opaque
/// context, passed back unchanged on every dispatch. `plugin` is the display
/// name of the owning extension (host-side only — stamped from the cdylib path
/// at registration; not part of the ABI), used for lifecycle veto diagnostics
/// ("extension `foo` aborted `before_tui_start`").
///
/// `priority`/`platforms` are the owning plugin's **declaration** (P2): the
/// snapshot sorts handlers by `priority` (smaller first, stable), and dispatch
/// skips a handler whose non-empty `platforms` excludes the host platform.
/// Both default to `100` / empty (all platforms); a plugin sets them via the
/// unified C ABI `declare` callback.
///
/// SAFETY contract: the plugin guarantees `handler` is safe to call from any
/// thread (the host dispatches from the async emitter thread) and `user_data`
/// is valid for the registry's lifetime. The host never frees `user_data`
/// (plugin-owned).
#[derive(Clone)]
pub struct RegisteredHandler {
    /// Display name of the owning extension (from the cdylib file stem).
    pub plugin: String,
    /// Dispatch order (smaller runs first). Default `100`.
    pub priority: i32,
    /// Platforms this handler's plugin supports (empty = all). Matched against
    /// [`current_platform`].
    pub platforms: Vec<String>,
    pub handler: EventHandlerFn,
    pub user_data: *mut std::ffi::c_void,
}
// SAFETY: fn pointers are Send+Sync; `user_data` is an opaque plugin pointer the
// plugin warrants is thread-safe to pass to `handler` from any thread. The host
// only reads it through `handler`.
unsafe impl Send for RegisteredHandler {}
unsafe impl Sync for RegisteredHandler {}

/// A registered `resources_discover` handler (B5b). The `out` [`StbString`] the
/// handler produces is **plugin-owned**, so the host reclaims it via the
/// plugin's own `plugin_free_string` traveled alongside. `user_data` is the
/// plugin's opaque context. SAFETY: same as [`RegisteredHandler`] — the plugin
/// warrants `handler` is callable from any thread and `user_data` is valid for
/// the registry's lifetime; the host never frees `user_data`.
#[derive(Clone, Copy)]
pub struct ResourcesDiscoverHandler {
    pub handler: ResourcesDiscoverFn,
    pub plugin_free_string: FreeStringFn,
    pub user_data: *mut std::ffi::c_void,
}
// SAFETY: fn pointers + an opaque plugin pointer the plugin warrants is
// thread-safe; the host only reads `user_data` through `handler`.
unsafe impl Send for ResourcesDiscoverHandler {}
unsafe impl Sync for ResourcesDiscoverHandler {}

/// A registered `before_agent_start` handler (system-prompt transform). Same
/// ownership shape as [`ResourcesDiscoverHandler`]: the handler produces a
/// **plugin-owned** `out` [`StbString`] the host reclaims via the plugin's own
/// `plugin_free_string`, which therefore travels alongside. `user_data` is the
/// plugin's opaque context.
///
/// SAFETY: same as [`RegisteredHandler`] — the plugin warrants `handler` is
/// callable from any thread and `user_data` is valid for the registry's
/// lifetime; the host never frees `user_data`.
#[derive(Clone, Copy)]
pub struct BeforeAgentStartHandler {
    pub handler: BeforeAgentStartFn,
    pub plugin_free_string: FreeStringFn,
    pub user_data: *mut std::ffi::c_void,
}
// SAFETY: fn pointers + an opaque plugin pointer the plugin warrants is
// thread-safe; the host only reads `user_data` through `handler`.
unsafe impl Send for BeforeAgentStartHandler {}
unsafe impl Sync for BeforeAgentStartHandler {}

/// A registered `tool_result` transform. Same ownership shape as
/// [`BeforeAgentStartHandler`]: the plugin-owned `out` is reclaimed through the
/// plugin's own `plugin_free_string`.
///
/// SAFETY: same as [`RegisteredHandler`].
#[derive(Clone, Copy)]
pub struct ToolResultHandler {
    pub handler: ToolResultFn,
    pub plugin_free_string: FreeStringFn,
    pub user_data: *mut std::ffi::c_void,
}
// SAFETY: fn pointers + an opaque plugin pointer the plugin warrants is
// thread-safe; the host only reads `user_data` through `handler`.
unsafe impl Send for ToolResultHandler {}
unsafe impl Sync for ToolResultHandler {}

/// A registered custom provider (B5c). The host wraps `request_fn` in a
/// [`PluggableProvider`](crate::PluggableProvider) impl of `rpi_ai::Provider`;
/// its `stream_simple` drives `request_fn` on `spawn_blocking` (the sync fn
/// can't own a chunked stream), reads the **plugin-owned** `out` JSON (a full
/// assistant message), reclaims it via `plugin_free_string`, parses it to an
/// [`AssistantMessage`](rpi_ai::types::AssistantMessage), and emits it as one
/// terminal `Done` chunk (v1 one-shot, documented divergence from pi's async
/// streaming). `provider_id`/`base_url`/`api_style` carry the provider's
/// identity (copied from the borrowed `StbStringRef`s at registration); the fn
/// pointers + `user_data` live as long as the plugin (keepalive-mapped).
/// `user_data` is the plugin's opaque context, passed back on every
/// `request_fn` call.
///
/// `stream_fn` is the **streaming** capability ([`ProviderStreamRequestFn`],
/// registered through `register_streaming_provider`). When present the host
/// drives it instead of `request_fn`: the plugin emits per-chunk envelopes that
/// the host converts into `AssistantMessageEvent`s *as they arrive*, then the
/// same plugin-owned `out` message still supplies the terminal event. `None`
/// keeps the v1 one-shot behavior, so an existing plugin is unaffected.
///
/// That single signature also carries the cancellation protocol: the host always
/// passes a [`ProviderStreamControl`](rpi_plugin_sdk::ProviderStreamControl), and
/// whether the plugin polls it is the plugin's choice. There is deliberately no
/// separate "cancellable" registration — one method, and the caller decides.
///
/// SAFETY: the plugin warrants `request_fn`/`stream_fn` are callable from any
/// thread (the host calls them from a `spawn_blocking` pool thread) and
/// `user_data` is valid for the plugin's lifetime; the host never frees
/// `user_data`.
#[derive(Clone)]
pub struct RegisteredProvider {
    pub provider_id: String,
    pub base_url: String,
    pub api_style: String,
    pub request_fn: ProviderRequestFn,
    /// Streaming request fn (streaming providers only).
    pub stream_fn: Option<ProviderStreamRequestFn>,
    pub plugin_free_string: FreeStringFn,
    pub user_data: *mut std::ffi::c_void,
}

/// A provider-owned OAuth implementation. `manifest_json` is validated JSON
/// copied from the plugin; the callback receives action envelopes such as
/// `{\"action\":\"begin\"}` and returns plugin-owned JSON.
#[derive(Clone)]
pub struct RegisteredOAuthProvider {
    /// The owning extension display name, used to scope credentials and
    /// diagnostics. This is host-owned metadata and is not supplied by the
    /// plugin.
    pub plugin: String,
    pub manifest_json: String,
    /// Legacy action callback. Used only when `control_fn` is `None`.
    pub request_fn: OAuthRequestFn,
    /// Cancellable action callback (registered through
    /// `register_oauth_provider_cancellable`). When present the host calls it
    /// instead of `request_fn`, with a control that reflects cancellation.
    pub control_fn: Option<OAuthRequestControlFn>,
    pub plugin_free_string: FreeStringFn,
    pub user_data: *mut std::ffi::c_void,
}
// SAFETY: as for RegisteredProvider, the plugin must keep its opaque context
// live and make request_fn safe to call from any host thread.
unsafe impl Send for RegisteredOAuthProvider {}
unsafe impl Sync for RegisteredOAuthProvider {}
// SAFETY: fn pointers + owned `String`s + an opaque plugin pointer the plugin
// warrants is thread-safe; the host never frees `user_data`.
unsafe impl Send for RegisteredProvider {}
unsafe impl Sync for RegisteredProvider {}

/// A registered message/markdown/entry renderer (B5c). The interactive TUI
/// consumes all three kinds through the JSON component adapter. `render_fn` produces a
/// **plugin-owned** `out` [`StbString`] the host reclaims via
/// `plugin_free_string`; `user_data` is passed back on every render call. `name`
/// is copied from the borrowed `StbStringRef` at registration.
///
/// SAFETY: same as [`RegisteredProvider`].
#[derive(Clone)]
pub struct RegisteredRenderer {
    pub name: String,
    pub kind: RegisteredRendererKind,
    pub render_fn: RenderFn,
    pub plugin_free_string: FreeStringFn,
    pub user_data: *mut std::ffi::c_void,
}

/// Which render path this renderer targets — mirrors the three distinct
/// `register_*` slots (`register_message_renderer` /
/// `register_markdown_transformer` / `register_entry_renderer`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RegisteredRendererKind {
    Message,
    Markdown,
    Entry,
}
// SAFETY: same as [`RegisteredProvider`] — fn pointers + owned `String` + an
// opaque plugin pointer the plugin warrants is thread-safe.
unsafe impl Send for RegisteredRenderer {}
unsafe impl Sync for RegisteredRenderer {}

/// One flat registration record, for iteration/diagnostics. Built on demand
/// from the typed vecs in [`RegistrySnapshot`].
#[derive(Clone)]
pub enum RegistryEntry {
    Tool(ExtensionTool),
    Command(RegisteredCommand),
    Flag(RegisteredFlag),
}

// ---------------------------------------------------------------------------
// P2 — plugin declarations (priority + platforms)
// ---------------------------------------------------------------------------

/// Default dispatch priority for a plugin that declares none. Handlers run
/// smallest-priority-first; ties break by registration order.
pub const DEFAULT_PRIORITY: i32 = 100;

/// The host platform, as matched against a plugin's declared `platforms`.
/// Platform set used by extension capability declarations.
pub fn current_platform() -> &'static str {
    if cfg!(target_os = "android") {
        "android"
    } else if cfg!(target_os = "linux") {
        // Termux reports `target_os = "android"` above; a plain Linux is here.
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    }
}

/// Whether a handler with the given declared `platforms` should run on this
/// host. Empty `platforms` = all platforms; a non-empty list requires the host
/// platform to be present (`"all"` is accepted as a wildcard).
pub fn platform_allows(platforms: &[String]) -> bool {
    if platforms.is_empty() {
        return true;
    }
    let here = current_platform();
    platforms
        .iter()
        .any(|p| p.eq_ignore_ascii_case("all") || p.eq_ignore_ascii_case(here))
}

// ---------------------------------------------------------------------------
// ExtensionRegistry — accumulated during register, then snapshotted
// ---------------------------------------------------------------------------

/// Accumulates registrations from one or more plugins' `rpi_plugin_register`
/// calls. Held by [`HostApi`](crate::HostApi) during register; the host then
/// [`take_registry`](crate::HostApi::take_registry) and builds a snapshot.
///
/// Merge semantics mirror pi: **first-registration-wins** on tool/command name
/// collision (a later registration for an existing name is dropped, not an
/// overwrite). Event handlers accumulate (multiple per tag — fan-out).
pub struct ExtensionRegistry {
    tools: Vec<ExtensionTool>,
    commands: Vec<RegisteredCommand>,
    flags: Vec<RegisteredFlag>,
    /// Keyboard shortcuts declared by plugins (first-wins by `key`). Recorded
    /// so the TUI can route a key press/release to the owning plugin's `Input`
    /// event handler instead of consuming it as editor text.
    shortcuts: Vec<RegisteredShortcut>,
    /// `handlers[tag as usize]` — all handlers subscribed to that tag.
    handlers: [Vec<RegisteredHandler>; EVENT_TAG_COUNT],
    /// `resources_discover` handlers (B5b). Fan-out on discovery, in registration
    /// order. Unlike event handlers (which key off a tag in a fixed array), these
    /// are a single flat list — `resources_discover` has its own out-param
    /// signature and is never dispatched through the fire-and-forget event path.
    resources_discover: Vec<ResourcesDiscoverHandler>,
    /// `before_agent_start` system-prompt transformers. A flat list for the same
    /// reason as `resources_discover`: this is an out-param handler (the plugin
    /// returns the prompt to install), not a fire-and-forget event subscription,
    /// so it is not indexed by a tag. Chained in registration order.
    before_agent_start: Vec<BeforeAgentStartHandler>,
    /// `tool_result` transforms, chained in registration order.
    tool_result: Vec<ToolResultHandler>,
    /// Registered custom providers (B5c). The host wraps each in a
    /// [`PluggableProvider`](crate::PluggableProvider). First-registration-wins on
    /// `provider_id`.
    providers: Vec<RegisteredProvider>,
    provider_manifests: std::collections::BTreeMap<String, ProviderManifest>,
    /// Provider-owned OAuth implementations. First-registration-wins on the
    /// manifest `id` field.
    oauth_providers: Vec<RegisteredOAuthProvider>,
    /// Registered renderers (B5c), split by kind at registration. First-wins on
    /// `(kind, name)`. The host records these now; TUI consumption is B5e.
    renderers: Vec<RegisteredRenderer>,
    /// P2: the registering plugin's declared dispatch priority. Stamped onto
    /// every handler it registers; a later `declare` also retro-updates them.
    priority: i32,
    /// P2: the registering plugin's declared platforms (empty = all).
    platforms: Vec<String>,
    /// A plugin's `{"ownsStdio":true}` claim: this host process's stdin/stdout
    /// belong to the plugin (an ACP/MCP-style stdio server), so the host must
    /// defer `read_piped_stdin` until after extension load and run the headless
    /// `Rpc` lifecycle instead of Print/TUI. Monotonic across `absorb` (any
    /// claiming plugin projects `true`). The scoped ActionBridge arbitrates
    /// exclusivity before registries are merged. Default `false`.
    owns_stdio: bool,
    /// Shared staleness flag. `true` while the session owning this registry is
    /// active; set `false` on swap/`/reload`. Tool/event dispatch checks it.
    active: Arc<AtomicBool>,
}

impl Default for ExtensionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionRegistry {
    /// Build an empty, active registry.
    pub fn new() -> Self {
        let handlers: [Vec<RegisteredHandler>; EVENT_TAG_COUNT] =
            std::array::from_fn(|_| Vec::new());
        Self {
            tools: Vec::new(),
            commands: Vec::new(),
            flags: Vec::new(),
            shortcuts: Vec::new(),
            handlers,
            resources_discover: Vec::new(),
            before_agent_start: Vec::new(),
            tool_result: Vec::new(),
            providers: Vec::new(),
            provider_manifests: std::collections::BTreeMap::new(),
            oauth_providers: Vec::new(),
            renderers: Vec::new(),
            priority: DEFAULT_PRIORITY,
            platforms: Vec::new(),
            owns_stdio: false,
            active: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Register a tool. Returns `true` if a **prior** tool of the same name was
    /// kept (first-wins: the new one is dropped). Returns `false` on insert.
    pub fn register_tool(&mut self, tool: Tool, handle: PluginToolHandle) -> bool {
        if self.tools.iter().any(|t| t.tool.name == tool.name) {
            // First-wins: keep the prior, drop the new handle. We must destroy
            // the dropped handle's allocation? No — `handle` is just fn pointers
            // + a FreeStringFn (all Copy, no allocation). Dropping it is a no-op.
            // A real per-tool plugin allocation only exists after `execute`
            // produces a StepHandle; we never call execute for a dropped
            // registration, so nothing to free.
            return true;
        }
        self.tools.push(ExtensionTool::new(tool, handle));
        false
    }

    /// Attach prompt metadata only to a tool in this plugin-local registry.
    /// Metadata follows the tool through first-wins merge and snapshot/reload.
    pub fn register_tool_prompt(&mut self, name: &str, prompt: ToolPromptMetadata) -> bool {
        let Some(tool) = self.tools.iter_mut().find(|tool| tool.tool.name == name) else {
            return false;
        };
        tool.prompt = prompt.normalized();
        true
    }

    /// Register a slash command (first-wins by name). `true` if a prior was kept.
    pub fn register_command(
        &mut self,
        name: String,
        description: String,
        handler: CommandHandlerFn,
        user_data: *mut std::ffi::c_void,
    ) -> bool {
        if self.commands.iter().any(|c| c.name == name) {
            return true;
        }
        self.commands.push(RegisteredCommand {
            name,
            description,
            handler,
            user_data,
        });
        false
    }

    /// Register a CLI flag (first-wins by name). Returns `true` when an
    /// earlier extension already declared the same flag.
    pub fn register_flag(&mut self, name: String, description: String) -> bool {
        if self.flags.iter().any(|flag| flag.name == name) {
            return true;
        }
        self.flags.push(RegisteredFlag { name, description });
        false
    }

    /// Declare a keyboard shortcut (first-wins by `key`). Returns `true` when an
    /// earlier extension already claimed the same key.
    ///
    /// Recording a shortcut does **not** bind the key to a callback: the owning
    /// plugin receives presses/releases through its `Input` event handler and
    /// decides what to do. The host uses this list only to know which keys are
    /// claimed, so it can route them to extensions instead of the editor.
    pub fn register_shortcut(&mut self, plugin: String, key: String, description: String) -> bool {
        if self.shortcuts.iter().any(|s| s.key == key) {
            return true;
        }
        self.shortcuts.push(RegisteredShortcut {
            key,
            description,
            plugin,
        });
        false
    }

    /// Subscribe a handler to `tag`. Multiple handlers per tag are kept
    /// (fan-out on dispatch). Always inserts; returns `false`. `plugin` is the
    /// owning extension's display name (host-side only, for diagnostics).
    pub fn register_event_handler(
        &mut self,
        plugin: String,
        tag: EventTag,
        handler: EventHandlerFn,
        user_data: *mut std::ffi::c_void,
    ) -> bool {
        let idx = tag as usize;
        if idx < EVENT_TAG_COUNT {
            // Stamp the registering plugin's current declaration (P2). A plugin
            // typically declares BEFORE registering handlers; `apply_declaration`
            // also retro-updates any handler registered before the declare call.
            self.handlers[idx].push(RegisteredHandler {
                plugin,
                priority: self.priority,
                platforms: self.platforms.clone(),
                handler,
                user_data,
            });
        }
        false
    }

    /// Apply a plugin's declaration parsed from the unified C ABI `declare` JSON
    /// (`{"priority":60,"platforms":["linux","macos"]}`). Unknown keys are
    /// ignored; malformed values leave the corresponding field unchanged.
    /// Retro-updates any handlers already registered (so declare order doesn't
    /// matter). Returns the number of fields applied.
    pub fn apply_declaration(
        &mut self,
        priority: Option<i32>,
        platforms: Option<Vec<String>>,
    ) -> usize {
        let mut applied = 0;
        if let Some(priority) = priority {
            self.priority = priority;
            for tag_handlers in self.handlers.iter_mut() {
                for h in tag_handlers.iter_mut() {
                    h.priority = priority;
                }
            }
            applied += 1;
        }
        if let Some(platforms) = platforms {
            self.platforms = platforms.clone();
            for tag_handlers in self.handlers.iter_mut() {
                for h in tag_handlers.iter_mut() {
                    h.platforms = platforms.clone();
                }
            }
            applied += 1;
        }
        applied
    }

    /// Parse a v3 `declare` JSON payload and apply it (P2). Recognised keys:
    /// `priority` (integer), `platforms` (array of strings) and `ownsStdio`
    /// (boolean; see [`ExtensionRegistry::declared_owns_stdio`]). Returns `true`
    /// if the payload parsed as JSON (even with no recognised keys), `false` on
    /// a parse error.
    pub fn apply_declaration_json(&mut self, json: &str) -> bool {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
            return false;
        };
        if let Some(providers) = value.get("providers") {
            let Ok(manifests) = serde_json::from_value::<Vec<ProviderManifest>>(providers.clone())
            else {
                return false;
            };
            if manifests.iter().any(|manifest| {
                manifest.id.trim().is_empty()
                    || manifest
                        .models
                        .iter()
                        .any(|model| model.id.trim().is_empty() || model.provider != manifest.id)
            }) {
                return false;
            }
            for manifest in manifests {
                self.provider_manifests
                    .insert(manifest.id.clone(), manifest);
            }
        }
        let priority = value
            .get("priority")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32);
        let platforms = value
            .get("platforms")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<String>>()
            });
        if let Some(owns) = value.get("ownsStdio").and_then(|v| v.as_bool()) {
            // A claim, not a toggle: once a plugin owns stdio it stays claimed
            // (and `absorb` ORs it across plugins).
            self.owns_stdio = self.owns_stdio || owns;
        }
        self.apply_declaration(priority, platforms);
        true
    }

    /// Register a `resources_discover` handler (B5b). Multiple handlers are kept
    /// (fan-out on discovery, registration order). Always inserts; returns `false`.
    pub fn register_resources_discover(
        &mut self,
        handler: ResourcesDiscoverFn,
        plugin_free_string: FreeStringFn,
        user_data: *mut std::ffi::c_void,
    ) -> bool {
        self.resources_discover.push(ResourcesDiscoverHandler {
            handler,
            plugin_free_string,
            user_data,
        });
        false
    }

    /// Register a `before_agent_start` system-prompt transformer. Multiple
    /// handlers are kept (chained in registration order: each sees the prompt the
    /// previous one returned, mirroring pi `runner.ts:1329-1347`). Always
    /// inserts; returns `false`.
    pub fn register_before_agent_start(
        &mut self,
        handler: BeforeAgentStartFn,
        plugin_free_string: FreeStringFn,
        user_data: *mut std::ffi::c_void,
    ) -> bool {
        self.before_agent_start.push(BeforeAgentStartHandler {
            handler,
            plugin_free_string,
            user_data,
        });
        false
    }

    /// Register a `tool_result` transform. Multiple handlers are kept and
    /// chained in registration order. Always inserts; returns `false`.
    pub fn register_tool_result(
        &mut self,
        handler: ToolResultFn,
        plugin_free_string: FreeStringFn,
        user_data: *mut std::ffi::c_void,
    ) -> bool {
        self.tool_result.push(ToolResultHandler {
            handler,
            plugin_free_string,
            user_data,
        });
        false
    }

    /// Register a custom provider (B5c). First-registration-wins on `provider_id`
    /// (a later registration for an existing id is dropped, mirroring tool/command
    /// first-wins). Returns `true` if a prior provider of the same id was kept.
    pub fn register_provider(&mut self, provider: RegisteredProvider) -> bool {
        if self
            .providers
            .iter()
            .any(|p| p.provider_id == provider.provider_id)
        {
            return true;
        }
        self.providers.push(provider);
        false
    }

    /// Register the **streaming** form of a custom provider (see
    /// [`RegisteredProvider::stream_fn`]).
    ///
    /// Differs from [`register_provider`](Self::register_provider) in one way:
    /// upgrading an id that already has a **one-shot** registration replaces it
    /// instead of being dropped, because streaming is a strict superset of the
    /// one-shot contract — losing the upgrade to first-wins would silently keep
    /// the plugin on the non-streaming path. A registration that arrives when the
    /// id already has a **streaming** fn is still dropped (first streaming
    /// registration wins). Returns `true` when an existing streaming provider
    /// was kept.
    pub fn register_streaming_provider(&mut self, provider: RegisteredProvider) -> bool {
        debug_assert!(
            provider.stream_fn.is_some(),
            "register_streaming_provider requires stream_fn; use register_provider otherwise"
        );
        if let Some(existing) = self
            .providers
            .iter_mut()
            .find(|p| p.provider_id == provider.provider_id)
        {
            if existing.stream_fn.is_some() {
                // Two streaming registrations for one id: first wins, like every
                // other registrar in this registry.
                return true;
            }
            // One-shot → streaming upgrade. Keep the record's slot (and its
            // position) but adopt the richer capability + any identity the
            // plugin re-sent.
            existing.stream_fn = provider.stream_fn;
            existing.request_fn = provider.request_fn;
            existing.base_url = provider.base_url;
            existing.api_style = provider.api_style;
            existing.plugin_free_string = provider.plugin_free_string;
            existing.user_data = provider.user_data;
            return false;
        }
        self.providers.push(provider);
        false
    }

    /// Register a provider-owned OAuth implementation. The manifest must be a
    /// JSON object with a non-empty string `id`; malformed manifests are
    /// rejected. Returns `true` when an existing provider won.
    pub fn register_oauth_provider(&mut self, provider: RegisteredOAuthProvider) -> bool {
        let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&provider.manifest_json)
        else {
            return true;
        };
        let Some(id) = manifest.get("id").and_then(serde_json::Value::as_str) else {
            return true;
        };
        if id.trim().is_empty()
            || self.oauth_providers.iter().any(|p| {
                serde_json::from_str::<serde_json::Value>(&p.manifest_json)
                    .ok()
                    .and_then(|v| {
                        v.get("id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    })
                    .as_deref()
                    == Some(id)
            })
        {
            return true;
        }
        self.oauth_providers.push(provider);
        false
    }

    /// Register a renderer (B5c — message/markdown/entry). First-wins on
    /// `(kind, name)`. Returns `true` if a prior renderer of the same kind+name
    /// was kept.
    pub fn register_renderer(&mut self, renderer: RegisteredRenderer) -> bool {
        if self
            .renderers
            .iter()
            .any(|r| r.kind == renderer.kind && r.name == renderer.name)
        {
            return true;
        }
        self.renderers.push(renderer);
        false
    }

    /// Build a snapshot the host session keeps. The registry's `active` flag is
    /// shared (Arc) so a later `invalidate` on the registry also invalidates the
    /// snapshot — important for cross-session staleness.
    pub fn snapshot(&self) -> RegistrySnapshot {
        // P2: dispatch order is priority-first (smaller first). `sort_by_key` is
        // stable, so equal priorities keep registration order (the prior
        // contract). Sorting per tag here means every snapshot — including the
        // merged session snapshot built via `absorb` — is pre-ordered.
        let mut handlers = self.handlers.clone();
        for tag_handlers in handlers.iter_mut() {
            tag_handlers.sort_by_key(|h| h.priority);
        }
        RegistrySnapshot {
            tools: self.tools.clone(),
            commands: self.commands.clone(),
            flags: self.flags.clone(),
            shortcuts: self.shortcuts.clone(),
            handlers,
            resources_discover: self.resources_discover.clone(),
            before_agent_start: self.before_agent_start.clone(),
            tool_result: self.tool_result.clone(),
            providers: self.providers.clone(),
            provider_manifests: self.provider_manifests.clone(),
            oauth_providers: self.oauth_providers.clone(),
            renderers: self.renderers.clone(),
            owns_stdio: self.owns_stdio,
            active: Arc::clone(&self.active),
        }
    }

    /// Mark this registry (and any snapshot sharing its flag) as stale.
    pub fn invalidate(&self) {
        self.active.store(false, Ordering::SeqCst);
    }

    /// Whether this registry is still active (not invalidated).
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// P2: the plugin's declared dispatch priority (default [`DEFAULT_PRIORITY`]).
    pub fn declared_priority(&self) -> i32 {
        self.priority
    }

    /// P2: the plugin's declared platforms (empty = all).
    pub fn declared_platforms(&self) -> &[String] {
        &self.platforms
    }

    /// Whether this plugin claimed ownership of the host process's stdio
    /// (`declare {"ownsStdio":true}`). Default `false`.
    pub fn declared_owns_stdio(&self) -> bool {
        self.owns_stdio
    }

    /// Absorb another registry's registrations into this one, first-wins on
    /// name (tools/commands) and appending (event handlers). Used by
    /// [`load_dir`](crate::loader::load_dir)/`merge_registries` to fold
    /// per-plugin registries into one session registry in load order. Consumes
    /// `other` (its `active` flag is discarded — the session registry's flag
    /// wins, since it owns session lifetime).
    pub(crate) fn absorb(&mut self, mut other: ExtensionRegistry) {
        for t in other.tools.drain(..) {
            if self.tools.iter().any(|x| x.tool.name == t.tool.name) {
                // first-wins: keep the prior, discard the new. Nothing to free
                // (handle is fn pointers). Emit no diagnostic here — the loader
                // may surface collisions if desired; v1 silent first-wins.
                continue;
            }
            self.tools.push(t);
        }
        for c in other.commands.drain(..) {
            if self.commands.iter().any(|x| x.name == c.name) {
                continue;
            }
            self.commands.push(c);
        }
        for flag in other.flags.drain(..) {
            if self.flags.iter().any(|existing| existing.name == flag.name) {
                continue;
            }
            self.flags.push(flag);
        }
        for shortcut in other.shortcuts.drain(..) {
            if self
                .shortcuts
                .iter()
                .any(|existing| existing.key == shortcut.key)
            {
                continue;
            }
            self.shortcuts.push(shortcut);
        }
        for (tag_idx, handlers) in other.handlers.iter_mut().enumerate() {
            self.handlers[tag_idx].append(handlers);
        }
        self.resources_discover
            .append(&mut other.resources_discover);
        self.before_agent_start
            .append(&mut other.before_agent_start);
        self.tool_result.append(&mut other.tool_result);
        for p in other.providers.drain(..) {
            if self
                .providers
                .iter()
                .any(|x| x.provider_id == p.provider_id)
            {
                continue;
            }
            if let Some(manifest) = other.provider_manifests.remove(&p.provider_id) {
                self.provider_manifests
                    .insert(p.provider_id.clone(), manifest);
            }
            self.providers.push(p);
        }
        for oauth in other.oauth_providers.drain(..) {
            if self.oauth_providers.iter().any(|existing| {
                let existing_id =
                    serde_json::from_str::<serde_json::Value>(&existing.manifest_json)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("id")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned)
                        });
                let oauth_id = serde_json::from_str::<serde_json::Value>(&oauth.manifest_json)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("id")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    });
                existing_id.is_some() && existing_id == oauth_id
            }) {
                continue;
            }
            self.oauth_providers.push(oauth);
        }
        for r in other.renderers.drain(..) {
            if self
                .renderers
                .iter()
                .any(|x| x.kind == r.kind && x.name == r.name)
            {
                continue;
            }
            self.renderers.push(r);
        }
        // stdio ownership is a claim: any claiming plugin makes the merged
        // session own stdio. The live bridge has already checked the exclusive
        // owner; this flag projects that claim into the immutable snapshot.
        self.owns_stdio |= other.owns_stdio;
    }
}

// ---------------------------------------------------------------------------
// RegistrySnapshot — the host session's immutable view
// ---------------------------------------------------------------------------

/// An immutable snapshot of an [`ExtensionRegistry`] the host session keeps for
/// its lifetime. Tools are wrapped in [`ExtensionTool`] so the host can build
/// [`PluginToolAdapter`](crate::PluginToolAdapter)s; event handlers are grouped
/// by tag for the [`ExtensionEmitter`](crate::ExtensionEmitter) to fan out.
///
/// The `active` flag is shared with the source registry, so invalidating the
/// registry (on session swap) invalidates this snapshot too.
pub struct RegistrySnapshot {
    tools: Vec<ExtensionTool>,
    commands: Vec<RegisteredCommand>,
    flags: Vec<RegisteredFlag>,
    shortcuts: Vec<RegisteredShortcut>,
    handlers: [Vec<RegisteredHandler>; EVENT_TAG_COUNT],
    resources_discover: Vec<ResourcesDiscoverHandler>,
    before_agent_start: Vec<BeforeAgentStartHandler>,
    tool_result: Vec<ToolResultHandler>,
    providers: Vec<RegisteredProvider>,
    provider_manifests: std::collections::BTreeMap<String, ProviderManifest>,
    oauth_providers: Vec<RegisteredOAuthProvider>,
    renderers: Vec<RegisteredRenderer>,
    /// A plugin's `{"ownsStdio":true}` claim (merged OR across plugins). The
    /// host reads this after `load_session` to defer `read_piped_stdin` and run
    /// the headless `Rpc` lifecycle. See [`ExtensionRegistry::declared_owns_stdio`].
    owns_stdio: bool,
    active: Arc<AtomicBool>,
}

impl RegistrySnapshot {
    /// The extension tools (schema + handle), insertion order.
    pub fn tools(&self) -> &[ExtensionTool] {
        &self.tools
    }

    /// Consume the snapshot into the owned tool list (for building adapters).
    pub fn into_tools(self) -> Vec<ExtensionTool> {
        self.tools
    }

    /// The registered slash commands.
    pub fn commands(&self) -> &[RegisteredCommand] {
        &self.commands
    }

    /// The registered CLI flags, insertion order.
    pub fn flags(&self) -> &[RegisteredFlag] {
        &self.flags
    }

    /// Whether any loaded plugin claimed ownership of the host process's stdio
    /// (`declare {"ownsStdio":true}`). The host uses this to defer
    /// `read_piped_stdin` and run the headless `Rpc` lifecycle.
    pub fn owns_stdio(&self) -> bool {
        self.owns_stdio
    }

    /// Keyboard shortcuts declared by extensions, insertion order.
    pub fn shortcuts(&self) -> &[RegisteredShortcut] {
        &self.shortcuts
    }

    /// Whether any extension has claimed `key` as a shortcut. Used by the TUI
    /// to route that key to extension `Input` events instead of the editor.
    pub fn has_shortcut(&self, key: &str) -> bool {
        self.shortcuts.iter().any(|s| s.key == key)
    }

    /// Handlers subscribed to `tag` (empty slice if none).
    pub fn handlers_for(&self, tag: EventTag) -> &[RegisteredHandler] {
        let idx = tag as usize;
        if idx < EVENT_TAG_COUNT {
            &self.handlers[idx]
        } else {
            &[]
        }
    }

    /// The `resources_discover` handlers, registration order (B5b). Empty when
    /// no plugin registered a discovery handler.
    pub fn resources_discover(&self) -> &[ResourcesDiscoverHandler] {
        &self.resources_discover
    }

    /// The `before_agent_start` system-prompt transformers, registration order.
    /// Empty when no plugin asked to transform the prompt — the common case, and
    /// the one callers treat as "leave the prompt alone".
    pub fn before_agent_start(&self) -> &[BeforeAgentStartHandler] {
        &self.before_agent_start
    }

    /// The `tool_result` transforms, registration order. Empty when no plugin
    /// registered one, in which case tools are not wrapped at all.
    pub fn tool_result(&self) -> &[ToolResultHandler] {
        &self.tool_result
    }

    /// The registered custom providers (B5c), registration order. The host wraps
    /// each in a [`PluggableProvider`](crate::PluggableProvider). Empty when no
    /// plugin registered a provider.
    pub fn providers(&self) -> &[RegisteredProvider] {
        &self.providers
    }

    /// Only declarations backed by a successfully registered provider are used.
    pub fn provider_manifest(&self, id: &str) -> Option<&ProviderManifest> {
        self.providers
            .iter()
            .any(|provider| provider.provider_id == id)
            .then(|| self.provider_manifests.get(id))
            .flatten()
    }

    /// All registered renderers (B5c), registration order, across all three
    /// kinds.
    pub fn renderers(&self) -> &[RegisteredRenderer] {
        &self.renderers
    }

    /// OAuth providers registered by extensions, in registration order.
    pub fn oauth_providers(&self) -> &[RegisteredOAuthProvider] {
        &self.oauth_providers
    }

    /// Find an OAuth provider by its manifest `id`.
    pub fn oauth_provider(&self, provider_id: &str) -> Option<&RegisteredOAuthProvider> {
        self.oauth_providers.iter().find(|provider| {
            serde_json::from_str::<serde_json::Value>(&provider.manifest_json)
                .ok()
                .and_then(|value| {
                    value
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(|id| id == provider_id)
                })
                .unwrap_or(false)
        })
    }

    /// The registered renderers of a specific kind (B5c). The TUI dispatches
    /// message and entry renderers through the same JSON adapter used by the
    /// markdown transformer.
    pub fn renderers_of(&self, kind: RegisteredRendererKind) -> Vec<RegisteredRenderer> {
        self.renderers
            .iter()
            .filter(|r| r.kind == kind)
            .cloned()
            .collect()
    }

    /// A flat iterator of all registrations (tools + commands), for diagnostics.
    pub fn entries(&self) -> Vec<RegistryEntry> {
        let mut v: Vec<RegistryEntry> = Vec::new();
        for t in &self.tools {
            v.push(RegistryEntry::Tool(t.clone()));
        }
        for c in &self.commands {
            v.push(RegistryEntry::Command(c.clone()));
        }
        for flag in &self.flags {
            v.push(RegistryEntry::Flag(flag.clone()));
        }
        v
    }

    /// Whether this snapshot's session is still active.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// The shared active flag (for [`assert_active`]).
    pub fn active_flag(&self) -> &Arc<AtomicBool> {
        &self.active
    }
}

/// Staleness guard: `true` while the owning session is still active. Called
/// before dispatching an extension event or driving an extension tool so a
/// stale registry (from a swapped-out session) can't act. Mirrors pi's
/// `ExtensionRuntimeState` active check.
pub fn assert_active(active: &Arc<AtomicBool>) -> bool {
    active.load(Ordering::SeqCst)
}

/// Build an `AgentError` for a stale-registry access.
#[allow(dead_code)]
pub(crate) fn stale_error() -> AgentError {
    AgentError::State("extensions registry is stale (session swapped/reloaded)".into())
}

// Suppress unused-fn warning until B3 wires dispatch through the snapshot.
#[allow(dead_code)]
fn _ensure_handler_accessor_used(snap: &RegistrySnapshot) {
    let _ = snap.handlers_for(EventTag::MessageEnd);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcuts_first_wins_and_merge() {
        let mut first = ExtensionRegistry::new();
        assert!(!first.register_shortcut("voice".into(), "space".into(), "talk".into()));
        // Second claim on the same key is rejected (first-wins).
        assert!(first.register_shortcut("other".into(), "space".into(), "also".into()));
        assert!(!first.register_shortcut("voice".into(), "enter".into(), "send".into()));

        let mut second = ExtensionRegistry::new();
        assert!(!second.register_shortcut("other".into(), "space".into(), "dup".into()));
        assert!(!second.register_shortcut("other".into(), "f1".into(), "help".into()));

        first.absorb(second);
        let snap = first.snapshot();
        assert_eq!(snap.shortcuts().len(), 3);
        assert_eq!(snap.shortcuts()[0].key, "space");
        assert_eq!(snap.shortcuts()[0].plugin, "voice");
        assert_eq!(snap.shortcuts()[1].key, "enter");
        assert_eq!(snap.shortcuts()[2].key, "f1");
        // The claimed-key query the TUI routes on.
        assert!(snap.has_shortcut("space"));
        assert!(!snap.has_shortcut("tab"));
    }

    #[test]
    fn cli_flags_snapshot_and_merge_first_wins() {
        let mut first = ExtensionRegistry::new();
        assert!(!first.register_flag("server".into(), "Start server".into()));
        assert!(first.register_flag("server".into(), "Different description".into()));

        let mut second = ExtensionRegistry::new();
        assert!(!second.register_flag("server".into(), "Second server".into()));
        assert!(!second.register_flag("port".into(), "Listen port".into()));

        first.absorb(second);
        let snapshot = first.snapshot();
        let flags = snapshot.flags();
        assert_eq!(flags.len(), 2);
        assert_eq!(flags[0].name, "server");
        assert_eq!(flags[0].description, "Start server");
        assert_eq!(flags[1].name, "port");
    }

    extern "C" fn noop_handler(
        _ev: rpi_plugin_sdk::StablePluginEvent,
        _ud: *mut std::ffi::c_void,
    ) -> i32 {
        0
    }

    fn handler_of<'a>(
        snap: &'a RegistrySnapshot,
        tag: EventTag,
        i: usize,
    ) -> &'a RegisteredHandler {
        &snap.handlers_for(tag)[i]
    }

    /// Two plugins declaring different priorities: the merged snapshot orders
    /// the lower-priority plugin's handler first (smaller runs first).
    #[test]
    fn priority_orders_handlers_after_merge() {
        let tag = EventTag::SessionStart;

        let mut late = ExtensionRegistry::new();
        late.apply_declaration(Some(200), None);
        late.register_event_handler("late".into(), tag, noop_handler, std::ptr::null_mut());

        let mut early = ExtensionRegistry::new();
        early.apply_declaration(Some(50), None);
        early.register_event_handler("early".into(), tag, noop_handler, std::ptr::null_mut());

        // Absorb in the "wrong" order — snapshot must still sort by priority.
        let mut session = ExtensionRegistry::new();
        session.absorb(late);
        session.absorb(early);
        let snap = session.snapshot();
        assert_eq!(handler_of(&snap, tag, 0).plugin, "early");
        assert_eq!(handler_of(&snap, tag, 1).plugin, "late");
    }

    /// Equal priorities keep registration order (stable sort).
    #[test]
    fn equal_priority_is_stable() {
        let tag = EventTag::TurnEnd;
        let mut reg = ExtensionRegistry::new();
        reg.register_event_handler("a".into(), tag, noop_handler, std::ptr::null_mut());
        reg.register_event_handler("b".into(), tag, noop_handler, std::ptr::null_mut());
        let snap = reg.snapshot();
        assert_eq!(handler_of(&snap, tag, 0).plugin, "a");
        assert_eq!(handler_of(&snap, tag, 1).plugin, "b");
    }

    /// A declaration applied AFTER handlers exist retro-updates them (declare
    /// order does not matter).
    #[test]
    fn declaration_retro_updates_existing_handlers() {
        let tag = EventTag::SessionStart;
        let mut reg = ExtensionRegistry::new();
        reg.register_event_handler("p".into(), tag, noop_handler, std::ptr::null_mut());
        assert_eq!(
            handler_of(&reg.snapshot(), tag, 0).priority,
            DEFAULT_PRIORITY
        );

        assert!(reg.apply_declaration_json(r#"{"priority":7,"platforms":["linux"]}"#));
        let snap = reg.snapshot();
        assert_eq!(handler_of(&snap, tag, 0).priority, 7);
        assert_eq!(
            handler_of(&snap, tag, 0).platforms,
            vec!["linux".to_string()]
        );
    }

    /// Malformed declaration JSON is rejected (returns `false`); recognised keys
    /// apply.
    #[test]
    fn apply_declaration_json_parses_and_rejects() {
        let mut reg = ExtensionRegistry::new();
        assert!(!reg.apply_declaration_json("not json"));
        assert!(reg.apply_declaration_json("{}"));
        assert!(reg.apply_declaration_json(r#"{"priority":3}"#));
        assert_eq!(reg.apply_declaration(Some(3), None), 1);
    }

    /// `declare {"ownsStdio":true}` claims the host process's stdio; it is
    /// monotonic (a later `false` does not un-claim) and merges by OR.
    #[test]
    fn declaration_owns_stdio_is_monotonic_and_merges() {
        let mut reg = ExtensionRegistry::new();
        assert!(!reg.declared_owns_stdio());
        assert!(!reg.snapshot().owns_stdio());

        assert!(reg.apply_declaration_json(r#"{"ownsStdio":true}"#));
        assert!(reg.declared_owns_stdio());
        assert!(reg.snapshot().owns_stdio());

        // A later `false` must not release an established claim.
        assert!(reg.apply_declaration_json(r#"{"ownsStdio":false}"#));
        assert!(reg.declared_owns_stdio());

        // `absorb` ORs ownership across plugins (any claiming plugin wins).
        let mut plain = ExtensionRegistry::new();
        let mut claiming = ExtensionRegistry::new();
        assert!(claiming.apply_declaration_json(r#"{"ownsStdio":true}"#));
        plain.absorb(claiming);
        assert!(plain.declared_owns_stdio());
        assert!(plain.snapshot().owns_stdio());
    }

    #[test]
    fn platform_allows_matches_host_and_wildcard() {
        assert!(platform_allows(&[]));
        assert!(platform_allows(&["all".to_string()]));
        assert!(platform_allows(&["ALL".to_string()]));
        assert!(platform_allows(&[current_platform().to_string()]));
        assert!(!platform_allows(&["nonexistent-os".to_string()]));
        // Empty host subset is fine; a non-matching list is not.
        assert!(!platform_allows(&["definitely-not-this-host".to_string()]));
    }

    extern "C" fn noop_oauth(
        _request: rpi_plugin_sdk::StbStringRef,
        _out: *mut rpi_plugin_sdk::StbString,
        _user_data: *mut std::ffi::c_void,
    ) -> i32 {
        0
    }

    extern "C" fn noop_free(_value: rpi_plugin_sdk::StbString) {}

    #[test]
    fn oauth_manifest_registers_and_deduplicates() {
        let mut reg = ExtensionRegistry::new();
        let make = |manifest: &str| RegisteredOAuthProvider {
            plugin: "test-plugin".to_string(),
            manifest_json: manifest.to_string(),
            request_fn: noop_oauth,
            control_fn: None,
            plugin_free_string: noop_free,
            user_data: std::ptr::null_mut(),
        };
        assert!(!reg.register_oauth_provider(make(r#"{"id":"acme","displayName":"Acme"}"#)));
        assert!(reg.register_oauth_provider(make(r#"{"id":"acme"}"#)));
        assert!(reg.register_oauth_provider(make(r#"{"displayName":"missing-id"}"#)));
        assert_eq!(reg.snapshot().oauth_providers().len(), 1);
    }
}

#[cfg(test)]
mod provider_manifest_tests {
    use super::*;
    use rpi_plugin_sdk::{StbString, StbStringRef};
    extern "C" fn request(_: StbStringRef, _: *mut StbString, _: *mut std::ffi::c_void) -> i32 {
        1
    }
    fn record() -> RegisteredProvider {
        RegisteredProvider {
            provider_id: "package".into(),
            base_url: "http://localhost".into(),
            api_style: "openai-completions".into(),
            request_fn: request,
            stream_fn: None,
            plugin_free_string: rpi_plugin_sdk::callbacks::free_string,
            user_data: std::ptr::null_mut(),
        }
    }
    fn declaration(id: &str, model_id: &str) -> String {
        serde_json::json!({"providers":[{"id":id,"models":[rpi_ai::Model::new(model_id, model_id,
            rpi_ai::Api::OpenaiCompletions,"package","http://localhost")]}]})
        .to_string()
    }
    #[test]
    fn catalog_requires_registration_and_stays_with_the_winning_provider() {
        let mut first = ExtensionRegistry::new();
        assert!(first.apply_declaration_json(&declaration("package", "first")));
        assert!(first.snapshot().provider_manifest("package").is_none());
        first.register_provider(record());
        let manifest = first
            .snapshot()
            .provider_manifest("package")
            .unwrap()
            .clone();
        assert!(
            manifest.requires_auth,
            "packages require authentication by default"
        );
        assert_eq!(manifest.models[0].id, "first");
        let mut second = ExtensionRegistry::new();
        second.register_provider(record());
        assert!(second.apply_declaration_json(&declaration("package", "second")));
        first.absorb(second);
        assert_eq!(
            first
                .snapshot()
                .provider_manifest("package")
                .unwrap()
                .models[0]
                .id,
            "first"
        );
    }
    #[test]
    fn a_package_cannot_attach_a_catalog_to_another_providers_identity() {
        let mut registry = ExtensionRegistry::new();
        registry.register_provider(record());
        assert!(!registry.apply_declaration_json(&declaration("other", "wrong")));
        assert!(registry.snapshot().provider_manifest("package").is_none());
    }
}
