//! B5b — `resources_discover` host-side dispatch.
//!
//! pi's `resources_discover` is the one extension event that **returns a
//! result to the host** (all other `on()` handlers are fire-and-forget). The
//! plugin hands back `{skillPaths?, promptPaths?, themePaths?}` — bare string
//! arrays — which the host merges across handlers and feeds into Part A's
//! loaders (the "三者同交付" coherence point: the plugin's discovered paths
//! reuse the same skill/prompt loaders as the static Part-A dirs).
//!
//! ## The signature problem and its fix
//!
//! The SDK's [`EventHandlerFn`] (`i32` return, no `out`) cannot express this
//! result return, so B5b adds a distinct [`ResourcesDiscoverFn`] with an owning
//! `out: *mut StbString` (mirror of [`RuntimeActionFn`]'s out-param shape). The
//! `out` StbString is **plugin-produced**, so the host reclaims it via the
//! plugin's own `plugin_free_string`, which traveled alongside the handler at
//! registration (stored in [`ResourcesDiscoverHandler`]). This ownership detail
//! is the load-bearing thing the fire-and-forget event signature cannot express.
//!
//! ## Fan-out semantics (mirrors pi `runner.ts:1156-1192`)
//!
//! Each registered handler is called in registration order with `{type, cwd,
//! reason}`. One handler's error (nonzero `rc`) or panic **does not abort the
//! fan-out** — the host logs + skips it and continues to the next handler.
//! Results are concatenated (bare strings; rpi does NOT track per-extension
//! `extensionPath` — v1 documented divergence: paths are unattributed).
//!
//! [`EventHandlerFn`]: rpi_plugin_sdk::EventHandlerFn
//! [`RuntimeActionFn`]: rpi_plugin_sdk::RuntimeActionFn

use std::panic::{catch_unwind, AssertUnwindSafe};

use rpi_plugin_sdk::{StbString, StbStringRef};

use crate::registry::{assert_active, RegistrySnapshot};

/// The merged `resources_discover` result across all handlers: bare string
/// arrays for skills, prompt-templates, and themes. `theme_paths` is collected
/// for parity but rpi has no theme system yet (accepted, ignored, documented).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DiscoveredResources {
    pub skill_paths: Vec<String>,
    pub prompt_paths: Vec<String>,
    pub theme_paths: Vec<String>,
}

/// Fan the `resources_discover` event out to every registered handler in
/// registration order and merge their returned paths.
///
/// `cwd` and `reason` (`"startup"` or `"reload"`) are passed to each handler.
/// Returns the concatenated [`DiscoveredResources`]. A stale registry (swapped-
/// out session) returns empty — the staleness guard is the same one event
/// dispatch uses ([`assert_active`]).
///
/// Per-handler errors (nonzero `rc`) and panics are logged and skipped; they do
/// NOT abort the fan-out (mirrors pi `runner.ts:1179-1188`). The `out`
/// StbString each handler produces is plugin-owned and reclaimed via that
/// handler's stored `plugin_free_string` before moving to the next handler.
pub fn emit_resources_discover(
    cwd: &str,
    reason: &str,
    snapshot: &RegistrySnapshot,
) -> DiscoveredResources {
    if !assert_active(snapshot.active_flag()) {
        return DiscoveredResources::default();
    }
    let handlers = snapshot.resources_discover();
    if handlers.is_empty() {
        return DiscoveredResources::default();
    }

    // Borrowed inputs for every call: cwd + reason as StbStringRef (the plugin
    // reads them during the call; we hold the &str on this stack). pi's event
    // envelope also carries `type: "resources_discover"` but the handler
    // signature passes cwd/reason directly (the type is implicit in the slot).
    let cwd_ref = StbStringRef::from_str(cwd);
    let reason_ref = StbStringRef::from_str(reason);

    let mut merged = DiscoveredResources::default();
    for h in handlers {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            call_one_handler(*h, cwd_ref, reason_ref)
        }));
        match outcome {
            Ok(Ok(paths)) => {
                merged.skill_paths.extend(paths.skill_paths);
                merged.prompt_paths.extend(paths.prompt_paths);
                merged.theme_paths.extend(paths.theme_paths);
            }
            Ok(Err(rc)) => {
                tracing::warn!(
                    rc,
                    "resources_discover handler returned nonzero — skipped (fan-out continues)"
                );
            }
            Err(_) => {
                tracing::error!(
                    "resources_discover handler panicked — skipped (fan-out continues)"
                );
            }
        }
    }
    merged
}

/// Call one handler and reclaim its `out` StbString. Returns the parsed paths
/// (`Ok(DiscoveredResources)`) on `rc==0`, or the nonzero `rc` on error. The
/// `out` string is always freed (via the handler's `plugin_free_string`) before
/// returning, whether or not parsing succeeded — a handler returning `0` with a
/// malformed payload still has its allocation reclaimed.
fn call_one_handler(
    h: crate::registry::ResourcesDiscoverHandler,
    cwd_ref: StbStringRef,
    reason_ref: StbStringRef,
) -> Result<DiscoveredResources, i32> {
    // The uninitialized `out` slot the handler writes into on success. pi's
    // contract: `rc==0` ⇒ `out` is a plugin-owned JSON string the host frees;
    // `rc!=0` ⇒ `out` is left untouched (the handler wrote nothing). We zero it
    // so a `rc==0` handler that forgets to write still yields an empty parse
    // rather than UB.
    let mut out = StbString::empty();
    let rc = (h.handler)(cwd_ref, reason_ref, &mut out, h.user_data);
    // SAFETY: the callback transfers its live output to its registered producer.
    let out = unsafe {
        rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(h.plugin_free_string))
    };
    if rc != 0 {
        // The owner also reclaims output a handler wrote before returning error.
        return Err(rc);
    }

    // Copy the plugin-owned bytes into a safe String, THEN free the plugin
    // allocation (the plugin owns the bytes; we must not keep a reference past
    // the free). `to_string_lossy` copies without freeing.
    let json = out.to_string_lossy();

    let paths = parse_discover_payload(&json);
    Ok(paths)
}

/// Parse a handler's `out` JSON payload `{skillPaths?, promptPaths?,
/// themePaths?}` (all bare string arrays, all optional). Missing fields default
/// to empty. A non-object or unparseable payload yields empty arrays (lenient —
/// one handler returning garbage does NOT poison the merged result).
fn parse_discover_payload(json: &str) -> DiscoveredResources {
    let mut out = DiscoveredResources::default();
    if json.trim().is_empty() {
        return out;
    }
    let value: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "resources_discover payload not valid JSON — treating as empty");
            return out;
        }
    };
    let obj = match value.as_object() {
        Some(o) => o,
        None => {
            tracing::warn!("resources_discover payload not a JSON object — treating as empty");
            return out;
        }
    };
    if let Some(arr) = obj.get("skillPaths").and_then(|v| v.as_array()) {
        out.skill_paths
            .extend(arr.iter().filter_map(|v| v.as_str()).map(str::to_string));
    }
    if let Some(arr) = obj.get("promptPaths").and_then(|v| v.as_array()) {
        out.prompt_paths
            .extend(arr.iter().filter_map(|v| v.as_str()).map(str::to_string));
    }
    if let Some(arr) = obj.get("themePaths").and_then(|v| v.as_array()) {
        out.theme_paths
            .extend(arr.iter().filter_map(|v| v.as_str()).map(str::to_string));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ExtensionRegistry;
    use rpi_plugin_sdk::callbacks::free_string as test_free;
    use rpi_plugin_sdk::ResourcesDiscoverFn;
    use std::cell::Cell;
    use std::sync::atomic::Ordering;

    // Discovery fan-out runs synchronously on the calling thread. Per-thread
    // counters keep the tests isolated without passing stack pointers via ABI.
    thread_local! { static CALLS: Cell<usize> = const { Cell::new(0) }; }

    fn bump() {
        CALLS.with(|count| count.set(count.get() + 1));
    }

    rpi_plugin_sdk::string_handler! {
        /// A handler that returns two skill paths + one prompt path + one theme path.
        fn two_skill_handler((_cwd, _reason), out, _ud) {
        bump();
        let json = serde_json::json!({
            "skillPaths": ["/a/SKILL.md", "/b/SKILL.md"],
            "promptPaths": ["/p/greet.md"],
            "themePaths": ["/t/dark.json"],
        })
        .to_string();
        i32::from(!out.write(json))
    } }

    rpi_plugin_sdk::string_handler! {
        /// A handler that returns only skill paths (prompt/theme omitted — lenient).
        fn skill_only_handler((_cwd, _reason), out, _ud) {
        bump();
        let json = r#"{"skillPaths":["/c/SKILL.md"]}"#.to_string();
        i32::from(!out.write(json))
    } }

    rpi_plugin_sdk::string_handler! {
        /// A handler that reports an error (nonzero) — must be skipped, fan-out
        /// continues, and its `out` (untouched/empty) is defensively freed.
        fn error_handler((_cwd, _reason), _out, _ud) {
        bump();
        42
    } }

    rpi_plugin_sdk::string_handler! {
        /// A handler that returns 0 but writes garbage JSON — lenient parse yields
        /// empty arrays (does NOT poison the merged result), and the allocation is
        /// still freed.
        fn garbage_handler((_cwd, _reason), out, _ud) {
        bump();
        i32::from(!out.write("not json {{{".to_string()))
    } }

    /// Build a snapshot with an isolated synchronous dispatch counter.
    fn reg_with(handlers: &[ResourcesDiscoverFn]) -> RegistrySnapshot {
        CALLS.with(|count| count.set(0));
        let mut reg = ExtensionRegistry::new();
        for h in handlers {
            reg.register_resources_discover(*h, test_free, std::ptr::null_mut());
        }
        reg.snapshot()
    }

    #[test]
    fn no_handlers_returns_empty() {
        let snap = reg_with(&[]);
        let r = emit_resources_discover("/cwd", "startup", &snap);
        assert!(r.skill_paths.is_empty());
        assert!(r.prompt_paths.is_empty());
        assert!(r.theme_paths.is_empty());
    }

    #[test]
    fn one_handler_merges_all_three_arrays() {
        let snap = reg_with(&[two_skill_handler]);
        let r = emit_resources_discover("/cwd", "startup", &snap);
        assert_eq!(r.skill_paths, ["/a/SKILL.md", "/b/SKILL.md"]);
        assert_eq!(r.prompt_paths, ["/p/greet.md"]);
        assert_eq!(r.theme_paths, ["/t/dark.json"]);
        assert_eq!(CALLS.with(Cell::get), 1);
    }

    #[test]
    fn multiple_handlers_concatenate_in_registration_order() {
        let snap = reg_with(&[two_skill_handler, skill_only_handler]);
        let r = emit_resources_discover("/cwd", "reload", &snap);
        assert_eq!(r.skill_paths, ["/a/SKILL.md", "/b/SKILL.md", "/c/SKILL.md"]);
        // The second handler returned no prompt/theme → only the first's.
        assert_eq!(r.prompt_paths, ["/p/greet.md"]);
        assert_eq!(r.theme_paths, ["/t/dark.json"]);
        assert_eq!(CALLS.with(Cell::get), 2);
    }

    #[test]
    fn error_handler_skipped_fan_out_continues() {
        let snap = reg_with(&[error_handler, two_skill_handler, garbage_handler]);
        let r = emit_resources_discover("/cwd", "startup", &snap);
        // error_handler skipped; two_skill_handler contributed;
        // garbage_handler produced empty (lenient). All three ran.
        assert_eq!(CALLS.with(Cell::get), 3);
        assert_eq!(r.skill_paths, ["/a/SKILL.md", "/b/SKILL.md"]);
        assert_eq!(r.prompt_paths, ["/p/greet.md"]);
    }

    #[test]
    fn stale_registry_returns_empty() {
        let snap = reg_with(&[two_skill_handler]);
        snap.active_flag().store(false, Ordering::SeqCst);
        let r = emit_resources_discover("/cwd", "startup", &snap);
        assert!(r.skill_paths.is_empty());
        assert_eq!(
            CALLS.with(Cell::get),
            0,
            "stale registry must not invoke handlers"
        );
    }

    #[test]
    fn parse_payload_lenient_defaults() {
        assert_eq!(parse_discover_payload(""), DiscoveredResources::default());
        assert_eq!(parse_discover_payload("{}"), DiscoveredResources::default());
        assert_eq!(
            parse_discover_payload(r#"{"skillPaths":["/x"]}"#).skill_paths,
            ["/x"]
        );
        // Non-object payloads collapse to empty, never panic.
        assert_eq!(
            parse_discover_payload("[1,2,3]"),
            DiscoveredResources::default()
        );
        assert_eq!(
            parse_discover_payload("null"),
            DiscoveredResources::default()
        );
    }
}
