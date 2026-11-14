//! The `libloading` loader: discover and register cdylib plugins.
//!
//! [`load_one`] loads a single cdylib and looks up the unified
//! `rpi_plugin_register` entrypoint (ABI v4). The entrypoint is called exactly
//! once; a nonzero return is reported via diagnostics.
//!
//! [`load_dir`] walks a directory for `.{dll,so,dylib}` files and loads each.
//! The returned [`LoadedPlugin`]s hold the `libloading::Library` (dropping them
//! shuts down resources before unloading — keep them alive for the session lifetime).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use libloading::Library;
use thiserror::Error;

use rpi_plugin_sdk::{RpiPluginRegisterUnified, RPI_PLUGIN_ABI_VERSION_UNIFIED};

use crate::registry::{ExtensionRegistry, RegistrySnapshot};
use crate::{enter_current_api, ActionBridge, HostApi, NullDiagnostics, PluginDiagnostics};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Error / skip reason from loading one plugin. `Skip` variants are non-fatal
/// (logged via diagnostics); `Fatal` means the load itself failed.
#[derive(Debug, Error)]
pub enum PluginLoadError {
    #[error("could not open library {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: libloading::Error,
    },
    #[error("`rpi_plugin_register` was not found in {path}: {error}")]
    Symbol { path: PathBuf, error: String },
    #[error("register returned nonzero code {code} for {path}")]
    RegisterReturned { path: PathBuf, code: i32 },
    /// ABI version reported by the plugin mismatches the host's. Skip + diag.
    #[error(
        "ABI version mismatch in {path}: plugin built for {plugin_version}, host is {host_version}"
    )]
    AbiVersionMismatch {
        path: PathBuf,
        plugin_version: u32,
        host_version: u32,
    },
}

// ---------------------------------------------------------------------------
// LoadedPlugin — the live cdylib handle + where it came from
// ---------------------------------------------------------------------------

/// A successfully loaded + registered plugin. Holds the `Library` so the cdylib
/// stays mapped for the session. Dropping this schedules final shutdown and
/// unloading (do not drop while any of its FFI calls may still be running).
pub struct LoadedPlugin {
    /// The loaded cdylib. Only final cleanup may release this handle.
    pub library: std::mem::ManuallyDrop<Library>,
    /// Where it was loaded from (for diagnostics).
    pub path: PathBuf,
    /// ABI version (always [`RPI_PLUGIN_ABI_VERSION_UNIFIED`] for plugins
    /// loaded by this version of the loader).
    pub abi_version: u32,
    /// The registry snapshot built from this plugin's registrations. The host
    /// merges snapshots from all loaded plugins into one session registry.
    pub registry: ExtensionRegistry,
    /// Retains the plugin-scoped action bridge used by its FFI callbacks.
    pub action_bridge: Option<Arc<ActionBridge>>,
    host_api: Option<Arc<HostApi>>,
    diagnostics: Arc<dyn PluginDiagnostics>,
    /// Host-only resource ownership, never part of the plugin SDK/ABI.
    host_guard: Option<Arc<dyn Send + Sync>>,
}

impl LoadedPlugin {
    /// Reject new background work and cancel existing work without waiting.
    pub fn begin_shutdown(&self) {
        if let Some(api) = &self.host_api {
            api.begin_shutdown();
        }
    }
}

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        // The last keepalive has gone: all FFI calls and plugin-owned string
        // frees have returned. Joining runtime workers must not block Tokio.
        let library = unsafe { std::mem::ManuallyDrop::take(&mut self.library) };
        let api = self.host_api.take();
        let diagnostics = self.diagnostics.clone();
        let path = self.path.clone();
        let host_guard = self.host_guard.take();
        let cleanup = move || {
            // Drop can run under a caller's lock. Plugin callbacks run only on
            // this cleanup thread; reload begins shutdown explicitly earlier.
            if let Some(api) = &api {
                api.begin_shutdown();
            }
            if api.as_ref().is_some_and(|api| !api.finish_shutdown()) {
                std::mem::forget(library);
                std::mem::forget(api);
                std::mem::forget(host_guard);
                diagnostics.warn(&format!(
                    "plugin {} shutdown failed; retaining its library and callback context for process lifetime",
                    path.display()
                ));
            } else {
                drop(library);
                drop(api);
                // Release exclusion only AFTER actual unmapping and callback
                // destruction, not when the asynchronous cleanup was queued.
                drop(host_guard);
            }
        };
        // If thread creation fails, dropping a captured Library would unload
        // live code. The shared cell lets the failure path retain everything.
        let pending = Arc::new(std::sync::Mutex::new(Some(cleanup)));
        let work = pending.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("plugin-shutdown".into())
            .spawn(move || {
                if let Some(cleanup) = work.lock().unwrap().take() {
                    cleanup();
                }
            })
        {
            std::mem::forget(pending);
            self.diagnostics.warn(&format!(
                "could not start plugin cleanup: {error}; retaining library"
            ));
        }
    }
}

fn call_register(register: RpiPluginRegisterUnified, host_api: &Arc<HostApi>) -> i32 {
    // Leak the API struct. The SDK asks plugins to *copy* the function pointers
    // they need during `register`, but real-world plugins retain the `api`
    // pointer to call e.g. `runtime_action` later. The host already keeps the
    // `user_data` (the `ActionBridge`) alive for the whole session, so keeping
    // the table valid is consistent with that contract and prevents a
    // use-after-free for plugins that hold the pointer. Bounded: one struct per
    // plugin load (a leak of a few dozen bytes per load/reload).
    //
    // Defense in depth: the SDK's `export_plugin!` (and `register_entrypoint`)
    // already convert a plugin panic into `REGISTER_PANIC_STATUS` before it can
    // reach the `extern "C"` boundary. We still wrap the call so a plugin built
    // against a *newer* `C-unwind` entrypoint, or a panic raised by host vtable
    // construction, is contained rather than unwinding through the loader.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let api: &'static rpi_plugin_sdk::PluginApi = Box::leak(Box::new(host_api.build_vtable()));
        register(api as *const rpi_plugin_sdk::PluginApi)
    })) {
        Ok(code) => code,
        // Reaching here means the panic unwound *into the host* rather than
        // aborting at the plugin's `extern "C"` frame (e.g. a future
        // `C-unwind` entrypoint). Report it the same way the SDK does.
        Err(_) => rpi_plugin_sdk::REGISTER_PANIC_STATUS,
    }
}

// ---------------------------------------------------------------------------
// load_one
// ---------------------------------------------------------------------------

/// Load and register one cdylib plugin. Returns the live plugin + its
/// registry, or a [`PluginLoadError`] (skip-fatality distinction is on the
/// caller; both are logged via `diagnostics`).
///
/// `diagnostics` is the host sink for ABI-mismatch/unsupported warnings. The
/// loader creates a fresh `ExtensionRegistry` for THIS plugin (so a plugin that
/// fails partway can't pollute others), and the caller merges per-plugin
/// registries into the session registry in load-order (first-wins on name).
///
/// `action_bridge` (B5a): when `Some`, the plugin's vtable wires the real
/// [`trampoline_runtime_action`] and carries the bridge in `user_data`, so the
/// plugin can invoke host runtime actions post-register from any thread. `None`
/// keeps the stub (actions return `-1`). `rpi-cli` builds ONE master
/// `Arc<ActionBridge>` per session. Each plugin gets a scoped bridge sharing
/// those facilities, retained alongside its library for the session lifetime.
pub fn load_one(
    path: impl AsRef<Path>,
    diagnostics: Arc<dyn PluginDiagnostics>,
    action_bridge: Option<Arc<ActionBridge>>,
) -> Result<LoadedPlugin, PluginLoadError> {
    load_one_guarded(path.as_ref(), diagnostics, action_bridge, None)
}

fn load_one_guarded(
    path: &Path,
    diagnostics: Arc<dyn PluginDiagnostics>,
    action_bridge: Option<Arc<ActionBridge>>,
    host_guard: Option<Arc<dyn Send + Sync>>,
) -> Result<LoadedPlugin, PluginLoadError> {
    let path = path.to_path_buf();
    // 1. Open the cdylib.
    let library = unsafe { Library::new(&path) }.map_err(|e| PluginLoadError::Open {
        path: path.clone(),
        source: e,
    })?;

    // 2. Look up the unified entrypoint.
    let register: RpiPluginRegisterUnified = unsafe {
        library
            .get::<RpiPluginRegisterUnified>(rpi_plugin_sdk::REGISTER_SYMBOL_UNIFIED)
            .map(|symbol| *symbol)
    }
    .map_err(|e| PluginLoadError::Symbol {
        path: path.clone(),
        error: e.to_string(),
    })?;
    let abi_version = RPI_PLUGIN_ABI_VERSION_UNIFIED;

    // Plugin display name (file stem) — stamped onto every registration the
    // plugin makes so host diagnostics (e.g. lifecycle veto messages) can name
    // the owning extension. Pure host-side; never crosses the ABI.
    let plugin_name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<unknown-plugin>".to_string());

    // 3. Build a fresh registry + HostApi + vtable for this plugin.
    let registry = ExtensionRegistry::new();
    let host_api = match action_bridge {
        Some(bridge) => HostApi::with_action_bridge(
            plugin_name.clone(),
            registry,
            Arc::clone(&diagnostics),
            bridge,
        ),
        None => HostApi::new(plugin_name, registry, Arc::clone(&diagnostics)),
    };
    // Install ownership before calling plugin code, including failed registrations.
    let mut loaded = LoadedPlugin {
        action_bridge: host_api.action_bridge.clone(),
        library: std::mem::ManuallyDrop::new(library),
        path: path.clone(),
        abi_version,
        registry: ExtensionRegistry::new(),
        host_api: Some(host_api.clone()),
        diagnostics: diagnostics.clone(),
        host_guard,
    };
    // 4. Own the thread-local api during registration and restore the enclosing
    //    context afterward, even if registration fails or reenters the loader.
    let rc = {
        let _registration = enter_current_api(&host_api);
        call_register(register, &host_api)
    };

    if rc != 0 {
        if let Some(bridge) = &host_api.action_bridge {
            bridge.release_stdio_claim();
        }
        // The plugin refused (its register returned nonzero — e.g. it saw an
        // ABI version it didn't like), or its body panicked and the SDK
        // converted that into `REGISTER_PANIC_STATUS`. Skip + diag. The
        // registry may have partial registrations; we drop it (no harm — the
        // tools registered so far would reference a plugin that "failed", so
        // we honor the refusal and discard).
        if rc == rpi_plugin_sdk::REGISTER_PANIC_STATUS {
            diagnostics.warn(&format!(
                "plugin {} panicked during registration — skipped (the host is unaffected)",
                path.display()
            ));
        } else {
            diagnostics.warn(&format!(
                "plugin {} ABI v{} register returned code {} — skipped",
                path.display(),
                abi_version,
                rc
            ));
        }
        return Err(PluginLoadError::RegisterReturned { path, code: rc });
    }

    // 5. Take the registry out of the HostApi. The host keeps the Library alive
    //    (LoadedPlugin) so the plugin's code + static data remain mapped; the
    //    registry holds fn pointers into that code.
    let registry = host_api
        .take_registry()
        .ok_or_else(|| PluginLoadError::RegisterReturned {
            path: path.clone(),
            code: -2,
        })?;

    tracing::debug!(path = %path.display(), abi_version, "loaded native plugin");

    loaded.registry = registry;
    Ok(loaded)
}

// ---------------------------------------------------------------------------
// load_dir
// ---------------------------------------------------------------------------

/// Platform cdylib extensions.
const CDYLIB_EXTS: &[&str] = &["dll", "so", "dylib", "pyd"];

/// Load every cdylib in `dir` (non-recursive). Each load failure is logged via
/// `diagnostics` and skipped (one bad plugin doesn't abort the rest). Returns
/// the successfully loaded plugins in directory order.
///
/// `action_bridge` (B5a) is cloned into each loaded plugin's vtable `user_data`
/// so post-register `runtime_action` calls recover the bridge on any thread.
pub fn load_dir(
    dir: impl AsRef<Path>,
    diagnostics: Arc<dyn PluginDiagnostics>,
    action_bridge: Option<Arc<ActionBridge>>,
) -> Vec<LoadedPlugin> {
    scan_dir(dir.as_ref(), diagnostics.as_ref())
        .into_iter()
        .filter_map(
            |path| match load_one(&path, Arc::clone(&diagnostics), action_bridge.clone()) {
                Ok(plugin) => Some(plugin),
                Err(error) => {
                    diagnostics.warn(&format!("skipped plugin {}: {error}", path.display()));
                    None
                }
            },
        )
        .collect()
}

fn scan_dir(dir: &Path, diagnostics: &dyn PluginDiagnostics) -> Vec<PathBuf> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| is_cdylib(path))
            .collect(),
        Err(error) => {
            diagnostics.warn(&format!(
                "extensions dir {} unreadable: {error}",
                dir.display()
            ));
            Vec::new()
        }
    }
}

/// Whether `path`'s extension is a known cdylib extension.
fn is_cdylib(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|ext| CDYLIB_EXTS.iter().any(|e| e.eq_ignore_ascii_case(ext)))
        .unwrap_or(false)
}

/// Convenience: merge a slice of per-plugin [`LoadedPlugin`] registries into one
/// session registry, first-wins on name (mirrors pi's cross-extension
/// registration order). Consumes the registries (the `LoadedPlugin`s themselves
/// stay alive — callers keep the `Library` handles).
pub fn merge_registries(plugins: &mut [LoadedPlugin]) -> ExtensionRegistry {
    let mut session = ExtensionRegistry::new();
    for p in plugins.iter_mut() {
        let taken = std::mem::take(&mut p.registry);
        session.absorb(taken);
    }
    session
}

// ---------------------------------------------------------------------------
// PluginKeepalive + ExtensionSession — the host session's plugin lifetime
// ---------------------------------------------------------------------------

/// Owns the loaded `Library` handles so the cdylibs stay mapped for as long as
/// any registered tool/handler (whose fn pointers live inside the cdylib) may be
/// called. Shared via `Arc`: every [`PluginToolAdapter`](crate::PluginToolAdapter)
/// (and, in B3, the [`ExtensionEmitter`](crate::ExtensionEmitter)) holds a clone,
/// so the libraries unload only when the last holder drops — which is never
/// before the harness's tool vec (and thus the last possible tool call) drops.
///
/// `libloading::Library` is `Send + Sync` (a handle/HMODULE), so the keepalive is
/// too — required because `AgentTool: Send + Sync` and the adapter carries it.
pub struct PluginKeepalive {
    #[allow(dead_code)]
    libraries: Vec<Library>,
    /// B5a: the session's action bridge. Retained here so the raw pointer a
    /// plugin stored in its vtable `user_data` (`Arc::as_ptr`) stays valid for
    /// the harness lifetime — every `PluginToolAdapter` + the `ExtensionEmitter`
    /// clone the keepalive, so the bridge outlives any plugin→host
    /// `runtime_action` call. `None` under `--no-extensions`, when zero plugins
    /// loaded, or in tests.
    #[allow(dead_code)]
    action_bridge: Option<Arc<ActionBridge>>,
    plugins: Vec<LoadedPlugin>,
}

impl PluginKeepalive {
    /// Build a keepalive. `pub` so tests + host code can construct an empty
    /// one (no loaded cdylibs) where the plugin lifecycle is exercised without
    /// real plugins.
    pub fn new(libraries: Vec<Library>, action_bridge: Option<Arc<ActionBridge>>) -> Self {
        Self {
            plugins: Vec::new(),
            libraries,
            action_bridge,
        }
    }

    /// Begin shutdown while outstanding adapters still retain the libraries.
    pub fn begin_shutdown(&self) {
        for plugin in &self.plugins {
            plugin.begin_shutdown();
        }
    }

    /// An empty keepalive owning no libraries — for host code that builds an
    /// adapter outside a real load session (notably in-process tests of the
    /// adapter against stub fns that live in the test binary, not a cdylib).
    pub fn empty() -> Arc<Self> {
        Arc::new(Self::new(Vec::new(), None))
    }
}

/// The result of loading a session's worth of extensions: a shared keepalive for
/// the cdylib handles + a snapshot of the merged registry. Built by
/// [`load_session`]; the host (pi-cli) stashes one per harness build and hands
/// clones of the keepalive to each adapter it constructs from the snapshot.
///
/// The snapshot is held behind `Arc` so the host can hand a clone to the
/// [`ExtensionEmitter`](crate::ExtensionEmitter) (installed as the harness's
/// `agent_emitter`) without borrowing — the emitter must outlive this session
/// local (it lives for the harness lifetime inside `AgentHarnessOptions`).
///
/// `Clone` (B5d): every field is already cheaply clonable (`Arc<PluginKeepalive>`,
/// `Option<Arc<RegistrySnapshot>>`, `Vec<PathBuf>`, `Option<Arc<ActionBridge>>`),
/// so the reload routine can clone the live session out of its `Mutex` cell for
/// local inspection (snapshot/keepalive/loaded_paths) and store a fresh one back
/// in — without a borrow spanning the store.
#[derive(Clone)]
pub struct ExtensionSession {
    keepalive: Arc<PluginKeepalive>,
    snapshot: Option<Arc<RegistrySnapshot>>,
    loaded_paths: Vec<PathBuf>,
    /// B5a: the session's action bridge (`None` when no plugins / tests / the
    /// `--no-extensions` path). Kept here so `rpi-cli` can recover it after
    /// `AgentHarness::create` to call `set_harness` — the bridge's `user_data`
    /// pointer was already handed out during `register`, so pi-cli must fill the
    /// host's harness cell immediately after create. Cloning is cheap (an `Arc`
    /// clone); the keepalive also holds a clone for the lifetime guarantee.
    action_bridge: Option<Arc<ActionBridge>>,
}

impl ExtensionSession {
    /// Assemble a session from already-loaded parts (explicit `--extension`
    /// files via `load_one` + `merge_registries`). Mirrors `load_session` but
    /// internal assembly so callers can build a session without a dir scan.
    pub fn from_parts(
        snapshot: Arc<RegistrySnapshot>,
        keepalive: Arc<PluginKeepalive>,
        loaded_paths: Vec<PathBuf>,
        action_bridge: Option<Arc<ActionBridge>>,
    ) -> Self {
        Self {
            keepalive,
            snapshot: Some(snapshot),
            loaded_paths,
            action_bridge,
        }
    }

    /// An empty session (no plugins loaded — `--no-extensions` or no dirs found).
    pub fn none() -> Self {
        Self {
            keepalive: Arc::new(PluginKeepalive::new(Vec::new(), None)),
            snapshot: None,
            loaded_paths: Vec::new(),
            action_bridge: None,
        }
    }

    /// Cancel plugin background work. Call outside host/session locks.
    pub fn begin_shutdown(&self) {
        self.keepalive.begin_shutdown();
    }

    /// The shared keepalive — clone one per adapter/emitter you build from this
    /// session so the cdylibs outlive them.
    pub fn keepalive(&self) -> Arc<PluginKeepalive> {
        Arc::clone(&self.keepalive)
    }

    /// The merged registry snapshot (tools/commands/handlers), if any plugin
    /// loaded. `None` when no plugins loaded successfully. Borrowed view for
    /// iterating tools/commands; for an owned share (e.g. handing to the
    /// emitter) use [`snapshot_arc`](Self::snapshot_arc).
    pub fn snapshot(&self) -> Option<&RegistrySnapshot> {
        self.snapshot.as_deref()
    }

    /// A shared (`Arc`) clone of the merged registry snapshot, for host code that
    /// must keep the snapshot alive beyond this session local — notably the
    /// [`ExtensionEmitter`](crate::ExtensionEmitter) installed into
    /// `AgentHarnessOptions.agent_emitter`.
    pub fn snapshot_arc(&self) -> Option<Arc<RegistrySnapshot>> {
        self.snapshot.clone()
    }

    /// Paths of the cdylibs that loaded + registered successfully (diagnostics).
    pub fn loaded_paths(&self) -> &[PathBuf] {
        &self.loaded_paths
    }

    /// Whether any loaded plugin claimed ownership of this host process's stdio
    /// (`declare {"ownsStdio":true}`). When `true`, the host (pi-cli) must not
    /// drain stdin before the plugin's `register` has run, and must run the
    /// headless `Rpc` lifecycle (dispatch BeforeTuiStart + SessionStart, then
    /// park) instead of Print/TUI so it never writes to the plugin's stdio.
    /// `false` for `ExtensionSession::none()` and for every non-claiming plugin.
    pub fn owns_stdio(&self) -> bool {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.owns_stdio())
            .unwrap_or(false)
    }

    /// Whether zero plugins loaded.
    pub fn is_empty(&self) -> bool {
        self.loaded_paths.is_empty()
    }

    /// A one-line human summary for `--verbose` startup output, or `None` when
    /// nothing loaded (so the line is omitted entirely).
    pub fn summary(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let tools = self.snapshot.as_ref().map(|s| s.tools().len()).unwrap_or(0);
        Some(format!(
            "loaded {} plugin(s) ({} tool(s))",
            self.loaded_paths.len(),
            tools
        ))
    }

    /// B5a: the session's action bridge, if one was threaded into `load_*`.
    /// `rpi-cli` recovers this after `AgentHarness::create` succeeds to call
    /// `HarnessActionHost::set_harness` (filling the host cell the bridge's
    /// `user_data`-recovered host reads on the first plugin→host action). The
    /// bridge pointer was already handed to plugins during `register`, so this
    /// must happen before any run. `None` when no plugins loaded / tests /
    /// `--no-extensions`.
    pub fn action_bridge(&self) -> Option<Arc<ActionBridge>> {
        self.action_bridge.clone()
    }
}

/// Load + register every cdylib in the given dirs (in order, non-recursive),
/// merge their registries first-wins, and return a session with a shared
/// keepalive over the `Library` handles + the merged snapshot. Dirs that don't
/// exist are skipped silently; individual plugin load failures are logged via
/// `diagnostics` and skipped (one bad plugin doesn't abort the rest).
///
/// The order of `dirs` matters: earlier dirs win on tool/command name collision
/// (pi registration order). Callers pass default dirs first, then `--extensions-dir`
/// extras, so a same-named tool in a default-dir plugin wins over an extra-dir one.
///
/// `action_bridge` (B5a) is cloned into every loaded plugin's vtable so
/// post-register `runtime_action` calls recover the bridge on any thread.
/// `rpi-cli` builds one master `Arc<ActionBridge>` per session and passes it
/// here; `None` keeps the stub (used by tests / `--no-extensions` no-ops).
pub fn load_session(
    dirs: &[PathBuf],
    diagnostics: Arc<dyn PluginDiagnostics>,
    action_bridge: Option<Arc<ActionBridge>>,
) -> ExtensionSession {
    load_session_mixed(dirs, &[], diagnostics, action_bridge)
}

/// Load plugins from a mix of scanned dirs and explicit cdylib files
/// (the `--extension`/`-e` CLI paths), assembled into one session. Mirrors
/// `load_session` but additionally `load_one`s each explicit file.
pub fn load_session_mixed(
    dirs: &[PathBuf],
    files: &[PathBuf],
    diagnostics: Arc<dyn PluginDiagnostics>,
    action_bridge: Option<Arc<ActionBridge>>,
) -> ExtensionSession {
    load_session_mixed_guarded(dirs, files, diagnostics, action_bridge, None)
}

/// Host-only ownership guard installed before registration, including failed
/// registration. Retained through async finalization, or leaked with unsafe code.
/// This is not a plugin SDK or ABI capability.
pub fn load_session_mixed_guarded(
    dirs: &[PathBuf],
    files: &[PathBuf],
    diagnostics: Arc<dyn PluginDiagnostics>,
    action_bridge: Option<Arc<ActionBridge>>,
    host_guard: Option<Arc<dyn Send + Sync>>,
) -> ExtensionSession {
    let mut loaded: Vec<LoadedPlugin> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let paths = dirs
        .iter()
        .flat_map(|dir| scan_dir(dir, diagnostics.as_ref()))
        .chain(files.iter().cloned());
    for path in paths {
        // Repeated configured dirs, explicit files and aliases must register once.
        let identity = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !seen.insert(identity) {
            continue;
        }
        match load_one_guarded(
            &path,
            Arc::clone(&diagnostics),
            action_bridge.clone(),
            host_guard.clone(),
        ) {
            Ok(plugin) => loaded.push(plugin),
            Err(error) => diagnostics.warn(&format!("skipped plugin {}: {error}", path.display())),
        }
    }
    if loaded.is_empty() {
        return ExtensionSession::none();
    }
    let loaded_paths: Vec<PathBuf> = loaded.iter().map(|p| p.path.clone()).collect();
    let session_registry = merge_registries(&mut loaded);
    let mut keepalive = PluginKeepalive::new(Vec::new(), action_bridge.clone());
    keepalive.plugins = loaded;
    let snapshot = Arc::new(session_registry.snapshot());
    ExtensionSession {
        keepalive: Arc::new(keepalive),
        snapshot: Some(snapshot),
        loaded_paths,
        action_bridge,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OAuthExtensionError;
    use std::fs;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    #[derive(Default)]
    struct CapturingDiag {
        warns: Mutex<Vec<String>>,
    }
    impl PluginDiagnostics for CapturingDiag {
        fn warn(&self, msg: &str) {
            self.warns.lock().unwrap().push(msg.to_string());
        }
        fn unsupported(&self, msg: &str) {
            self.warn(msg);
        }
    }

    struct CdylibFixture {
        dir: PathBuf,
        path: PathBuf,
    }

    impl Drop for CdylibFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn build_cdylib_fixture(name: &str, source: &str) -> CdylibFixture {
        static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT_FIXTURE.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::var_os("RPI_TEST_GUARD_PROCESS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!(
                "rpi-abi-loader-{}-{name}-{unique}",
                std::process::id()
            ));
        fs::create_dir_all(&dir).expect("create ABI fixture directory");
        let source_path = dir.join("fixture.rs");
        fs::write(&source_path, source).expect("write ABI fixture source");
        let filename = if cfg!(windows) {
            format!("{name}.dll")
        } else if cfg!(target_os = "macos") {
            format!("lib{name}.dylib")
        } else {
            format!("lib{name}.so")
        };
        let path = dir.join(filename);
        let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .arg("--crate-name")
            .arg(name)
            .arg("--crate-type")
            .arg("cdylib")
            .arg("--edition")
            .arg("2021")
            .arg(&source_path)
            .arg("-o")
            .arg(&path)
            .output()
            .expect("run rustc for ABI fixture");
        assert!(
            output.status.success(),
            "fixture build failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        CdylibFixture { dir, path }
    }

    fn lifecycle_fixture(
        name: &str,
        register_status: i32,
        finish_status: i32,
    ) -> (CdylibFixture, PathBuf) {
        lifecycle_fixture_gated(name, register_status, finish_status, None)
    }

    fn lifecycle_fixture_gated(
        name: &str,
        register_status: i32,
        finish_status: i32,
        gate: Option<&Path>,
    ) -> (CdylibFixture, PathBuf) {
        let finish_gate=gate.map(|p|format!("mark(b\"E\"); while !std::path::Path::new({:?}).exists() {{ std::thread::sleep(std::time::Duration::from_millis(1)); }}",p.to_string_lossy())).unwrap_or_default();
        let marker = gate
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("rpi-lifecycle-{}-{name}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let offset = std::mem::offset_of!(rpi_plugin_sdk::PluginApi, register_plugin_lifecycle);
        let fixture = build_cdylib_fixture(
            name,
            &format!(
                r#"
use std::ffi::c_void;
use std::sync::{{Arc, Mutex, atomic::{{AtomicBool, Ordering}}}};
static WORKER: Mutex<Option<(Arc<AtomicBool>, std::thread::JoinHandle<()>)>> = Mutex::new(None);
fn mark(value: &[u8]) {{
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open({marker:?}).unwrap();
    file.write_all(value).unwrap();
}}
extern "C" fn begin(_: *mut c_void) {{
    mark(b"B");
    WORKER.lock().unwrap().as_ref().unwrap().0.store(true, Ordering::Release);
}}
extern "C" fn finish(_: *mut c_void) -> i32 {{
    let (_, worker) = WORKER.lock().unwrap().take().unwrap();
    worker.join().unwrap();
    {finish_gate}
    mark(b"F");
    {finish_status}
}}
#[no_mangle]
pub extern "C" fn still_mapped() -> i32 {{ 42 }}
#[no_mangle]
pub extern "C" fn rpi_plugin_register(api: *const u8) -> i32 {{
    type Register = extern "C" fn(extern "C" fn(*mut c_void), extern "C" fn(*mut c_void) -> i32, *mut c_void) -> i32;
    let register: Option<Register> = unsafe {{ std::ptr::read_unaligned(api.add({offset}).cast()) }};
    assert_eq!(register.unwrap()(begin, finish, std::ptr::null_mut()), 0);
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = cancelled.clone();
    let worker = std::thread::spawn(move || {{
        while !worker_cancelled.load(Ordering::Acquire) {{
            std::thread::sleep(std::time::Duration::from_millis(1));
        }}
        mark(b"W");
    }});
    *WORKER.lock().unwrap() = Some((cancelled, worker));
    {register_status}
}}
"#,
                marker = marker.to_string_lossy()
            ),
        );
        (fixture, marker)
    }

    fn wait_for_finish(marker: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while fs::read(marker).unwrap_or_default() != b"BWF" {
            assert!(
                std::time::Instant::now() < deadline,
                "shutdown did not finish"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[cfg(windows)]
    fn dll_is_mapped(path: &Path) -> bool {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn GetModuleHandleW(name: *const u16) -> *mut std::ffi::c_void;
        }
        let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // GetModuleHandleW observes the loader without retaining another DLL reference.
        !unsafe { GetModuleHandleW(name.as_ptr()) }.is_null()
    }

    #[cfg(windows)]
    fn wait_for_unload(path: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while dll_is_mapped(path) {
            assert!(
                std::time::Instant::now() < deadline,
                "DLL remained mapped after successful shutdown"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn host_guard_covers_inflight_keepalives_async_cleanup_and_unsafe_shutdown() {
        const CHILD_DIR: &str = "RPI_TEST_GUARD_PROCESS_DIR";
        let Some(directory) = std::env::var_os(CHILD_DIR) else {
            for failed in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let output=Command::new(std::env::current_exe().unwrap())
                    .args(["--exact","loader::tests::host_guard_covers_inflight_keepalives_async_cleanup_and_unsafe_shutdown","--nocapture"])
                    .env(CHILD_DIR,directory.path()).env("RPI_TEST_GUARD_FAIL",failed.to_string()).output().unwrap();
                // Child process exit releases even intentionally leaked DLLs.
                // This parent owns and explicitly removes every fixture byte.
                directory
                    .close()
                    .expect("remove child lifecycle fixtures after process exit");
                assert!(
                    output.status.success(),
                    "lifecycle child failed: {}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        };
        for failed in [std::env::var("RPI_TEST_GUARD_FAIL").unwrap() == "true"] {
            let gate = PathBuf::from(directory.clone()).join("finish-gate");
            struct ReleaseGate(PathBuf);
            impl Drop for ReleaseGate {
                fn drop(&mut self) {
                    let _ = fs::write(&self.0, b"release");
                }
            }
            let _ = fs::remove_file(&gate);
            let (fixture, marker) = lifecycle_fixture_gated(
                if failed {
                    "guard_leaked"
                } else {
                    "guard_released"
                },
                0,
                i32::from(failed),
                Some(&gate),
            );
            // Declared after the fixture: panic always opens the finish gate,
            // and the parent removes files after this process has exited.
            let _release = ReleaseGate(gate.clone());
            let guard = Arc::new(());
            let weak = Arc::downgrade(&guard);
            let session = load_session_mixed_guarded(
                &[],
                std::slice::from_ref(&fixture.path),
                Arc::new(NullDiagnostics),
                None,
                Some(guard.clone()),
            );
            drop(guard);
            // An adapter/provider/callback holds this same keepalive while FFI
            // is in flight. Dropping the session must not release exclusion.
            let callback = session.keepalive();
            drop(session);
            assert!(weak.upgrade().is_some());
            assert!(!marker.exists());
            drop(callback);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !fs::read(&marker).unwrap_or_default().contains(&b'E') {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            assert!(
                weak.upgrade().is_some(),
                "cleanup is blocked; the ownership guard must still be retained"
            );
            #[cfg(windows)]
            assert!(dll_is_mapped(&fixture.path));
            fs::write(&gate, b"release").unwrap();
            if failed {
                while !fs::read(&marker).unwrap_or_default().contains(&b'F') {
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
                assert!(
                    weak.upgrade().is_some(),
                    "unsafe shutdown must leak exclusion with the DLL"
                );
                #[cfg(windows)]
                assert!(dll_is_mapped(&fixture.path));
            } else {
                while weak.upgrade().is_some() {
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                #[cfg(windows)]
                assert!(!dll_is_mapped(&fixture.path));
            }
            let _ = fs::remove_file(&gate);
            let _ = fs::remove_file(marker);
        }
    }

    #[test]
    fn lifecycle_cancels_once_and_waits_for_last_keepalive() {
        let (fixture, marker) = lifecycle_fixture("lifecycle_keepalive", 0, 0);
        let session = load_session_mixed(
            &[],
            std::slice::from_ref(&fixture.path),
            Arc::new(NullDiagnostics),
            None,
        );
        let active_request = session.keepalive();
        #[cfg(windows)]
        assert!(dll_is_mapped(&fixture.path));
        session.begin_shutdown();
        session.begin_shutdown();
        drop(session);
        let recorded = fs::read(&marker).unwrap();
        assert_eq!(recorded.iter().filter(|&&event| event == b'B').count(), 1);
        assert!(
            !recorded.contains(&b'F'),
            "finish ran while an FFI keepalive remained"
        );
        drop(active_request);
        wait_for_finish(&marker);
        #[cfg(windows)]
        wait_for_unload(&fixture.path);
        let _ = fs::remove_file(marker);
    }

    #[test]
    fn failed_registration_also_finishes_lifecycle() {
        let (fixture, marker) = lifecycle_fixture("lifecycle_register_failure", 73, 0);
        assert!(matches!(
            load_one(&fixture.path, Arc::new(NullDiagnostics), None),
            Err(PluginLoadError::RegisterReturned { code: 73, .. })
        ));
        wait_for_finish(&marker);
        let _ = fs::remove_file(marker);
    }

    #[test]
    fn failed_finish_keeps_dll_mapped_and_logs() {
        let (fixture, marker) = lifecycle_fixture("lifecycle_finish_failure", 0, 1);
        let diagnostics = Arc::new(CapturingDiag::default());
        let plugin = load_one(&fixture.path, diagnostics.clone(), None).unwrap();
        let still_mapped = unsafe {
            *plugin
                .library
                .get::<extern "C" fn() -> i32>(b"still_mapped\0")
                .unwrap()
        };
        drop(plugin);
        wait_for_finish(&marker);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while diagnostics.warns.lock().unwrap().is_empty() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(still_mapped(), 42);
        assert!(diagnostics.warns.lock().unwrap()[0].contains("retaining"));
        let _ = fs::remove_file(marker);
    }

    #[test]
    fn loads_unified_plugin() {
        const PREFIX: &str = "use std::ffi::c_void;\n";
        let diag: Arc<dyn PluginDiagnostics> = Arc::new(CapturingDiag::default());

        let unified = build_cdylib_fixture(
            "abi_unified_only",
            &format!(
                "{PREFIX}#[no_mangle]\npub extern \"C\" fn rpi_plugin_register(api: *const c_void) -> i32 {{ if !api.is_null() {{ 0 }} else {{ 91 }} }}\n"
            ),
        );
        let loaded = load_one(&unified.path, Arc::clone(&diag), None).expect("load unified plugin");
        assert_eq!(loaded.abi_version, RPI_PLUGIN_ABI_VERSION_UNIFIED);
        drop(loaded);
        drop(unified);
    }

    #[test]
    fn unified_registration_failure_is_reported() {
        const PREFIX: &str = "use std::ffi::c_void;\n";
        let diag: Arc<dyn PluginDiagnostics> = Arc::new(CapturingDiag::default());

        let failed = build_cdylib_fixture(
            "abi_unified_failure",
            &format!(
                "{PREFIX}#[no_mangle]\npub extern \"C\" fn rpi_plugin_register(_: *const c_void) -> i32 {{ 73 }}\n"
            ),
        );
        let error = match load_one(&failed.path, diag, None) {
            Ok(_) => panic!("failed registration must be reported as error"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            PluginLoadError::RegisterReturned { code: 73, .. }
        ));
        drop(failed);
    }

    /// A plugin whose `extern "C"` register body panics must be skipped, not
    /// crash the host. This mirrors what the SDK's `export_plugin!` /
    /// `register_entrypoint` do: catch the panic *inside* the plugin and return
    /// `REGISTER_PANIC_STATUS` so the unwind never reaches the `extern "C"`
    /// boundary (which would abort the process).
    #[test]
    fn panicking_register_body_is_contained_and_skipped() {
        let diag = Arc::new(CapturingDiag::default());
        let diag_dyn: Arc<dyn PluginDiagnostics> = diag.clone();
        let panic_status = rpi_plugin_sdk::REGISTER_PANIC_STATUS;
        const PREFIX: &str = "use std::ffi::c_void;\n";
        let source = format!(
            r#"{PREFIX}
fn body() -> i32 {{ panic!("register body exploded"); }}
#[no_mangle]
pub extern "C" fn rpi_plugin_register(_: *const c_void) -> i32 {{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {{
        Ok(code) => code,
        Err(_) => {panic_status},
    }}
}}
"#
        );
        let fixture = build_cdylib_fixture("abi_unified_panic", &source);
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = load_one(&fixture.path, diag_dyn, None);
        std::panic::set_hook(prev_hook);

        match result {
            Ok(_) => panic!("a panicking register must be skipped, not loaded"),
            Err(PluginLoadError::RegisterReturned { code, .. }) => {
                assert_eq!(code, panic_status);
            }
            Err(other) => panic!("expected RegisterReturned, got {other:?}"),
        }
        let warns = diag.warns.lock().unwrap().clone();
        assert!(
            warns
                .iter()
                .any(|w| w.contains("panicked during registration")),
            "diagnostic should mention the panic: {warns:?}"
        );
        drop(fixture);
    }

    #[test]
    fn repeated_discovery_and_explicit_alias_register_the_library_once() {
        let fixture = build_cdylib_fixture(
            "register_once",
            r#"
use std::sync::atomic::{AtomicUsize, Ordering};
static COUNT: AtomicUsize = AtomicUsize::new(0);
#[no_mangle]
pub extern "C" fn rpi_plugin_register(_: *const std::ffi::c_void) -> i32 {
    if COUNT.fetch_add(1, Ordering::SeqCst) == 0 { 0 } else { 73 }
}
"#,
        );
        let diag = Arc::new(CapturingDiag::default());
        let alias = fixture
            .dir
            .join(".")
            .join(fixture.path.file_name().unwrap());
        let session = load_session_mixed(
            &[fixture.dir.clone(), fixture.dir.clone()],
            &[alias],
            diag.clone(),
            None,
        );
        assert_eq!(session.loaded_paths().len(), 1);
        assert!(diag.warns.lock().unwrap().is_empty());
        drop(session);
    }

    #[test]
    fn load_one_missing_file_reports_open_error() {
        let diag: Arc<dyn PluginDiagnostics> = Arc::new(CapturingDiag::default());
        let res = load_one("definitely_not_a_plugin.dll", diag, None);
        assert!(matches!(res, Err(PluginLoadError::Open { .. })));
    }

    #[test]
    fn load_dir_missing_dir_returns_empty_and_warns() {
        let empty = load_dir(
            "no_such_dir_xyz",
            Arc::new(CapturingDiag::default()) as Arc<dyn PluginDiagnostics>,
            None,
        );
        assert!(empty.is_empty());
    }

    /// Prelude shared by the OAuth ABI fixtures. A foreign plugin sees the host
    /// types only through their layout, so they are spelled out here by hand.
    const OAUTH_FIXTURE_PRELUDE: &str = r#"
use std::ffi::c_void;
#[repr(C)] #[derive(Clone, Copy)] struct Str { ptr: *const c_void, len: usize }
#[repr(C)] #[derive(Clone, Copy)] struct Owned { ptr: *mut c_void, len: usize }
extern "C" fn free_output(_: Owned) {}
const MANIFEST: &[u8] = b"{\"id\":\"acme\"}";
fn mark(path: &str, bytes: &[u8]) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}
"#;

    /// An older plugin registers through the legacy `register_oauth_provider`
    /// slot. The current host must still dispatch to it, with no control.
    #[test]
    fn legacy_oauth_slot_from_an_older_plugin_reaches_the_host() {
        let marker = std::env::temp_dir().join(format!("rpi-oauth-legacy-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let offset = std::mem::offset_of!(rpi_plugin_sdk::PluginApi, register_oauth_provider);
        let fixture = build_cdylib_fixture(
            "oauth_legacy_slot",
            &format!(
                r#"{OAUTH_FIXTURE_PRELUDE}
extern "C" fn legacy(_req: Str, _out: *mut Owned, _data: *mut c_void) -> i32 {{
    mark({marker:?}, b"L");
    9
}}
#[no_mangle]
pub extern "C" fn rpi_plugin_register(api: *const u8) -> i32 {{
    type Register = extern "C" fn(Str, extern "C" fn(Str, *mut Owned, *mut c_void) -> i32,
        extern "C" fn(Owned), *mut c_void) -> i32;
    let register: Option<Register> = unsafe {{ std::ptr::read_unaligned(api.add({offset}).cast()) }};
    let manifest = Str {{ ptr: MANIFEST.as_ptr().cast(), len: MANIFEST.len() }};
    register.unwrap()(manifest, legacy, free_output, std::ptr::null_mut())
}}
"#
            ),
        );
        let session = load_session_mixed(
            &[],
            std::slice::from_ref(&fixture.path),
            Arc::new(NullDiagnostics),
            None,
        );
        let snapshot = session.snapshot_arc().expect("legacy fixture registers");
        let provider = snapshot.oauth_provider("acme").expect("legacy provider");
        assert!(provider.control_fn.is_none());
        let result = crate::request_oauth(&snapshot, "acme", "begin", serde_json::json!({}));
        assert!(
            matches!(result, Err(OAuthExtensionError::PluginStatus(9, _))),
            "{result:?}"
        );
        assert_eq!(fs::read(&marker).unwrap(), b"L");
        let _ = fs::remove_file(marker);
    }

    /// A current plugin on the `register_oauth_provider_cancellable` slot. Each
    /// call marks "C", blocks until the host cancels it, then marks "K".
    fn cancellable_oauth_fixture(name: &str, marker: &Path) -> CdylibFixture {
        let offset = std::mem::offset_of!(
            rpi_plugin_sdk::PluginApi,
            register_oauth_provider_cancellable
        );
        build_cdylib_fixture(
            name,
            &format!(
                r#"{OAUTH_FIXTURE_PRELUDE}
#[repr(C)]
struct Control {{
    struct_size: u32,
    is_cancelled: extern "C" fn(*mut c_void) -> i32,
    user_data: *mut c_void,
}}
extern "C" fn controlled(_req: Str, control: *const Control, _out: *mut Owned, _data: *mut c_void) -> i32 {{
    let control = unsafe {{ &*control }};
    if (control.struct_size as usize) < std::mem::size_of::<Control>() {{
        mark({marker:?}, b"S");
        return 2;
    }}
    mark({marker:?}, b"C");
    let started = std::time::Instant::now();
    while (control.is_cancelled)(control.user_data) == 0 {{
        if started.elapsed() > std::time::Duration::from_secs(10) {{
            mark({marker:?}, b"T");
            return 3;
        }}
        std::thread::sleep(std::time::Duration::from_millis(1));
    }}
    mark({marker:?}, b"K");
    9
}}
#[no_mangle]
pub extern "C" fn rpi_plugin_register(api: *const u8) -> i32 {{
    type Register = extern "C" fn(Str, extern "C" fn(Str, *const Control, *mut Owned, *mut c_void) -> i32,
        extern "C" fn(Owned), *mut c_void) -> i32;
    let register: Option<Register> = unsafe {{ std::ptr::read_unaligned(api.add({offset}).cast()) }};
    let manifest = Str {{ ptr: MANIFEST.as_ptr().cast(), len: MANIFEST.len() }};
    register.unwrap()(manifest, controlled, free_output, std::ptr::null_mut())
}}
"#
            ),
        )
    }

    /// A current plugin registers through `register_oauth_provider_cancellable`.
    /// Its `exchange` blocks until the host cancels a request that is in flight,
    /// across the real cdylib boundary.
    #[test]
    fn cancellable_oauth_slot_delivers_host_cancellation_across_the_abi() {
        let marker = std::env::temp_dir().join(format!("rpi-oauth-cancel-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let fixture = cancellable_oauth_fixture("oauth_cancellable_slot", &marker);
        let session = load_session_mixed(
            &[],
            std::slice::from_ref(&fixture.path),
            Arc::new(NullDiagnostics),
            None,
        );
        let snapshot = session
            .snapshot_arc()
            .expect("cancellable fixture registers");
        assert!(snapshot
            .oauth_provider("acme")
            .expect("cancellable provider")
            .control_fn
            .is_some());
        let cancel = tokio_util::sync::CancellationToken::new();
        let trigger = cancel.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            trigger.cancel();
        });
        let started = std::time::Instant::now();
        let result = crate::request_oauth_with_cancel(
            &snapshot,
            "acme",
            "exchange",
            serde_json::json!({"value": "code"}),
            &cancel,
        );
        let elapsed = started.elapsed();
        canceller.join().unwrap();
        assert!(
            matches!(result, Err(OAuthExtensionError::Cancelled)),
            "{result:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "took {elapsed:?}"
        );
        // "C": the plugin saw a sized control. "K": it observed the cancellation.
        assert_eq!(fs::read(&marker).unwrap(), b"CK");
        let _ = fs::remove_file(marker);
    }

    /// Reload while a cancellable call is in flight: the call keeps the old
    /// session's keepalive until it returns, cancellation still reaches it, and
    /// the reloaded snapshot dispatches a new call to a live plugin. (The OS may
    /// share one mapping for the same path; the keepalive count is what is pinned.)
    #[test]
    fn cancellable_oauth_call_survives_a_reload_and_still_cancels() {
        let marker = std::env::temp_dir().join(format!("rpi-oauth-reload-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let fixture = cancellable_oauth_fixture("oauth_cancellable_reload", &marker);
        let load = || {
            load_session_mixed(
                &[],
                std::slice::from_ref(&fixture.path),
                Arc::new(NullDiagnostics),
                None,
            )
        };
        let old = load();
        let (snapshot, keepalive) = (old.snapshot_arc().unwrap(), old.keepalive());
        let old_keepalive = Arc::downgrade(&keepalive);
        let cancel = tokio_util::sync::CancellationToken::new();
        let worker_cancel = cancel.clone();
        // Holds the keepalive across the call, as the TUI's blocking task does.
        let worker = std::thread::spawn(move || {
            let _keepalive = keepalive;
            crate::request_oauth_with_cancel(
                &snapshot,
                "acme",
                "exchange",
                serde_json::json!({"value": "code"}),
                &worker_cancel,
            )
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while fs::read(&marker).unwrap_or_default() != b"C" {
            assert!(std::time::Instant::now() < deadline, "call never started");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        old.begin_shutdown();
        drop(old);
        let reloaded = load();
        assert!(
            old_keepalive.strong_count() > 0,
            "the reload released the old session under the in-flight call"
        );
        cancel.cancel();
        let result = worker.join().unwrap();
        assert!(
            matches!(result, Err(OAuthExtensionError::Cancelled)),
            "{result:?}"
        );
        assert_eq!(fs::read(&marker).unwrap(), b"CK");
        assert_eq!(
            old_keepalive.strong_count(),
            0,
            "the old keepalive outlived the cancelled call"
        );

        // The reloaded snapshot serves a new call, which a cancel still stops.
        let new_snapshot = reloaded.snapshot_arc().expect("reloaded plugin registers");
        let new_cancel = tokio_util::sync::CancellationToken::new();
        let new_worker_cancel = new_cancel.clone();
        let new_worker = std::thread::spawn(move || {
            crate::request_oauth_with_cancel(
                &new_snapshot,
                "acme",
                "exchange",
                serde_json::json!({"value": "code"}),
                &new_worker_cancel,
            )
        });
        wait_for_marker(&marker, b"CKC");
        new_cancel.cancel();
        let result = new_worker.join().unwrap();
        assert!(
            matches!(result, Err(OAuthExtensionError::Cancelled)),
            "{result:?}"
        );
        assert_eq!(fs::read(&marker).unwrap(), b"CKCK");

        let precancelled = tokio_util::sync::CancellationToken::new();
        precancelled.cancel();
        let result = crate::request_oauth_with_cancel(
            &reloaded.snapshot_arc().expect("reloaded plugin registers"),
            "acme",
            "begin",
            serde_json::json!({}),
            &precancelled,
        );
        assert!(
            matches!(result, Err(OAuthExtensionError::Cancelled)),
            "{result:?}"
        );
        assert_eq!(
            fs::read(&marker).unwrap(),
            b"CKCK",
            "a cancelled call reached the reloaded plugin"
        );
        let _ = fs::remove_file(marker);
    }

    /// Waits until the fixture's marker file holds exactly `expected`.
    fn wait_for_marker(marker: &Path, expected: &[u8]) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while fs::read(marker).unwrap_or_default() != expected {
            assert!(
                std::time::Instant::now() < deadline,
                "marker never reached {expected:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Unload, with no replacement session, while a cancellable call is in
    /// flight. The caller's keepalive clone keeps the library mapped: the plugin
    /// still runs after the session is dropped, observes the cancellation, and
    /// the keepalive is released only once the call has returned.
    #[test]
    fn cancellable_oauth_call_survives_an_unload_and_releases_the_library_after_return() {
        let marker = std::env::temp_dir().join(format!("rpi-oauth-unload-{}", std::process::id()));
        let _ = fs::remove_file(&marker);
        let fixture = cancellable_oauth_fixture("oauth_cancellable_unload", &marker);
        let session = load_session_mixed(
            &[],
            std::slice::from_ref(&fixture.path),
            Arc::new(NullDiagnostics),
            None,
        );
        let snapshot = session
            .snapshot_arc()
            .expect("cancellable fixture registers");
        let keepalive = session.keepalive();
        let released = Arc::downgrade(&keepalive);
        let cancel = tokio_util::sync::CancellationToken::new();
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || {
            let _keepalive = keepalive;
            crate::request_oauth_with_cancel(
                &snapshot,
                "acme",
                "exchange",
                serde_json::json!({"value": "code"}),
                &worker_cancel,
            )
        });
        wait_for_marker(&marker, b"C");
        session.begin_shutdown();
        drop(session);
        assert!(
            released.strong_count() > 0,
            "unloading the session released the library under the in-flight call"
        );
        cancel.cancel();
        let result = worker.join().unwrap();
        assert!(
            matches!(result, Err(OAuthExtensionError::Cancelled)),
            "{result:?}"
        );
        // "K": the unloaded plugin still ran and observed the cancellation.
        assert_eq!(fs::read(&marker).unwrap(), b"CK");
        assert_eq!(
            released.strong_count(),
            0,
            "the keepalive outlived the cancelled call"
        );
        let _ = fs::remove_file(marker);
    }

    /// An older plugin on the legacy `register_oauth_provider` slot. Its
    /// `exchange` marks "S", waits for the `release` file, then marks "L" and
    /// returns a credential. It cannot observe the host's cancellation.
    fn legacy_oauth_fixture(name: &str, marker: &Path, release: &Path) -> CdylibFixture {
        let offset = std::mem::offset_of!(rpi_plugin_sdk::PluginApi, register_oauth_provider);
        build_cdylib_fixture(
            name,
            &format!(
                r#"{OAUTH_FIXTURE_PRELUDE}
extern "C" fn legacy(_req: Str, out: *mut Owned, _data: *mut c_void) -> i32 {{
    mark({marker:?}, b"S");
    let started = std::time::Instant::now();
    while !std::path::Path::new({release:?}).exists() {{
        if started.elapsed() > std::time::Duration::from_secs(10) {{
            mark({marker:?}, b"T");
            return 3;
        }}
        std::thread::sleep(std::time::Duration::from_millis(1));
    }}
    mark({marker:?}, b"L");
    let body: &'static [u8] = b"{{\"version\":1,\"kind\":\"credential\",\"payload\":{{\"access\":\"late\"}}}}";
    unsafe {{ *out = Owned {{ ptr: body.as_ptr() as *mut c_void, len: body.len() }}; }}
    0
}}
#[no_mangle]
pub extern "C" fn rpi_plugin_register(api: *const u8) -> i32 {{
    type Register = extern "C" fn(Str, extern "C" fn(Str, *mut Owned, *mut c_void) -> i32,
        extern "C" fn(Owned), *mut c_void) -> i32;
    let register: Option<Register> = unsafe {{ std::ptr::read_unaligned(api.add({offset}).cast()) }};
    let manifest = Str {{ ptr: MANIFEST.as_ptr().cast(), len: MANIFEST.len() }};
    register.unwrap()(manifest, legacy, free_output, std::ptr::null_mut())
}}
"#
            ),
        )
    }

    /// Compatibility path: an older plugin's call is in flight when the session
    /// is unloaded and then cancelled. The host cannot interrupt the plugin, so
    /// it must keep the library mapped until the plugin returns, and the late
    /// credential it produces must come back as `Cancelled`, never as a result.
    #[test]
    fn legacy_oauth_call_in_flight_across_an_unload_returns_cancelled() {
        let pid = std::process::id();
        let marker = std::env::temp_dir().join(format!("rpi-oauth-legacy-unload-{pid}"));
        let release = std::env::temp_dir().join(format!("rpi-oauth-legacy-release-{pid}"));
        let _ = fs::remove_file(&marker);
        let _ = fs::remove_file(&release);
        let fixture = legacy_oauth_fixture("oauth_legacy_unload", &marker, &release);
        let session = load_session_mixed(
            &[],
            std::slice::from_ref(&fixture.path),
            Arc::new(NullDiagnostics),
            None,
        );
        let snapshot = session.snapshot_arc().expect("legacy fixture registers");
        let keepalive = session.keepalive();
        let released = Arc::downgrade(&keepalive);
        let cancel = tokio_util::sync::CancellationToken::new();
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || {
            let _keepalive = keepalive;
            crate::request_oauth_with_cancel(
                &snapshot,
                "acme",
                "exchange",
                serde_json::json!({"value": "code"}),
                &worker_cancel,
            )
        });
        wait_for_marker(&marker, b"S");
        session.begin_shutdown();
        drop(session);
        assert!(
            released.strong_count() > 0,
            "unloading the session released the library under the in-flight call"
        );
        cancel.cancel();
        fs::write(&release, b"").unwrap();
        let result = worker.join().unwrap();
        assert!(
            matches!(result, Err(OAuthExtensionError::Cancelled)),
            "{result:?}"
        );
        assert_eq!(fs::read(&marker).unwrap(), b"SL");
        assert_eq!(
            released.strong_count(),
            0,
            "the keepalive outlived the call"
        );
        let _ = fs::remove_file(marker);
        let _ = fs::remove_file(release);
    }

    #[test]
    fn is_cdylib_recognizes_extensions() {
        assert!(is_cdylib(Path::new("foo.dll")));
        assert!(is_cdylib(Path::new("foo.so")));
        assert!(is_cdylib(Path::new("foo.dylib")));
        assert!(is_cdylib(Path::new("FOO.DLL")));
        assert!(!is_cdylib(Path::new("foo.md")));
        assert!(!is_cdylib(Path::new("foo")));
    }
}

// Silence the unused-default-import warning for NullDiagnostics re-exported by the crate.
#[allow(dead_code)]
fn _ensure_nulldiagnostics_referenced() -> Arc<dyn PluginDiagnostics> {
    Arc::new(NullDiagnostics)
}
