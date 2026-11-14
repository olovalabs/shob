//! `plugin-stub` — a minimal **Rust cdylib** rpi plugin.
//!
//! This is the Part B2 ABI smoke target. The host loads it via `rpi-extensions`
//! (`--extensions-dir examples/plugin-stub`'), which looks up
//! `rpi_plugin_register` (the unified ABI) and calls it with the host vtable. In `register` this
//! plugin:
//!
//! 1. Registers an **`echo`** tool — params `{ "text": string }`, returns that
//!    text prefixed with `echo: `. It drives the full 4-function lifecycle
//!    (`execute`→handle, `poll`, `cancel`, `destroy`) so the host's
//!    `PluginToolAdapter` spawn_blocking bridge is exercised end-to-end against
//!    a real cdylib (not just the in-process test stub).
//! 2. Registers one **event handler** on `MessageEnd` that records the last
//!    message it saw into a process-global slot — proving the 33-category `on()`
//!    dispatch path is wired for at least one tag (the full fan-out to every
//!    `AgentEvent`-folded tag lands in B3).
//! 3. Registers a **`resources_discover`** handler (B5b) that hands back a
//!    canned skill path — proving the discover→host round-trip (the host fans
//!    the event out to every registered handler, the plugin returns
//!    `{skillPaths:[...]}`, the host merges the arrays and feeds Part A's skill
//!    loader). This is the "三者同交付" coherence point: a plugin's discovered
//!    paths reuse the same loader as static `.pi/skills` dirs.
//!
//! It depends on **`rpi-plugin-sdk` only** — never on `rpi-extensions`,
//! `rpi-harness`, or `rpi-cli` (a cdylib plugin must not link the host; the host
//! links the plugin). The SDK is the ABI contract both sides share.
//!
//! ## Run
//!
//! ```sh
//! cargo build -p plugin-stub
//! rpi --extensions-dir examples/plugin-stub/target/.../echo.dll -p 'echo "hi"'
//! ```
//! (`echo.dll`/`libecho.so`/`libecho.dylib` depending on platform — the loader
//! recognizes all of `.{dll,so,dylib,pyd}`.)

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use rpi_plugin_sdk::{
    EventTag, ProviderChunkKind, ProviderStreamCompletionFn, ProviderStreamControl,
    ProviderStreamEmitFn, StableToolSchema, StbString, StbStringRef, StepHandle, StepResult,
    StepResultTag, ToolPartialCb,
};
// The provider/render fn signatures are referenced transitively by the
// `register_provider`/`register_markdown_transformer` vtable slots (the host
// passes our `extern "C" fn`s as `ProviderRequestFn`/`RenderFn`-typed params).
// Naming the types here keeps the import live and documents the contract.
use rpi_plugin_sdk::callbacks::free_string as plugin_free_string;
use rpi_plugin_sdk::handles::ToolHandles;
#[allow(unused_imports)]
use rpi_plugin_sdk::{ProviderRequestFn, ProviderStreamRequestFn, RenderFn};

// ---------------------------------------------------------------------------
// The echo tool's per-drive state — owned by the safe opaque-handle registry.
// ---------------------------------------------------------------------------

/// One echo drive. `polls` counts how many times `poll` was called; we return
/// `Done` after the second poll (echo is a single-shot tool — there's no real async
/// work to drive). Kept slightly non-trivial so the bridge's poll loop + the
/// partial-callback path are both touched.
struct EchoDrive {
    text: String,
    polls: usize,
}

static ECHO_DRIVES: ToolHandles<EchoDrive> = ToolHandles::new();

// ---------------------------------------------------------------------------
// The echo tool's 4 lifecycle fns (extern "C", no captured state)
// ---------------------------------------------------------------------------

rpi_plugin_sdk::execute_handler! {
    /// `execute(tool_call_id, params, free_params) -> StepHandle`.
    ///
    /// The SDK adopts host-owned parameters and reclaims them through the host's
    /// `free_params`. Parse `text` (missing ⇒ empty), then allocate drive state.
    fn echo_execute(_tool_call_id, params) {
        let text = params.to_string_lossy();
        let parsed: serde_json::Value =
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        let value = parsed
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        ECHO_DRIVES.insert(EchoDrive {
            text: value,
            polls: 0,
        })
    }
}

/// `poll(handle, partial_cb, user_data) -> StepResult`. **Non-blocking.**
///
/// First poll: emit a `Pending` partial via the callback ("..."), then return
/// `Pending` once more with an empty progress (exercising the inline-progress
/// path too). Second poll: return `Done` with the echoed text.
///
/// `partial_cb` is invoked **synchronously inside poll** (the SDK contract).
/// `user_data` is the host-provided pointer bound to the callback; pass it
/// through unchanged.
extern "C" fn echo_poll(
    handle: StepHandle,
    partial_cb: Option<ToolPartialCb>,
    user_data: *mut c_void,
) -> StepResult {
    // Contain panics so a buggy poll reports a tool error instead of aborting
    // the host process.
    rpi_plugin_sdk::guard(|| {
        let Some(entry) = ECHO_DRIVES.get(handle) else {
            return StepResult::err(StbString::from_string(
                r#"{"error":"invalid or destroyed echo handle"}"#.into(),
            ));
        };
        if entry.cancelled.load(Ordering::Acquire) {
            return StepResult::err(StbString::from_string(
                r#"{"error":"echo cancelled"}"#.into(),
            ));
        }
        let text = {
            let mut drive = entry.lock();
            drive.polls += 1;
            (drive.polls > 1).then(|| drive.text.clone())
        };
        // Do not hold the state lock while calling back into the host.
        let Some(text) = text else {
            // Emit a partial progress result through the callback path.
            if let Some(cb) = partial_cb {
                let progress = StbString::from_string(
                    r#"{"content":[{"type":"text","text":"..."}]}"#.to_string(),
                );
                // The host frees the partial's StbString (it is plugin-produced here,
                // and the host reclaims it through our registered plugin_free_string).
                cb(progress, user_data);
            }
            // Return Pending with empty inline progress (separate from the callback
            // partial already sent). Empty StbString → the host skips it.
            return StepResult::pending(StbString::empty());
        };

        // Second poll: terminal Done with the echoed text.
        let body = format!(
            r#"{{"content":[{{"type":"text","text":"echo: {}"}}]}}"#,
            escape_json_string(&text)
        );
        StepResult::done(StbString::from_string(body))
    })
    .unwrap_or_else(|_| {
        StepResult::err(StbString::from_string(
            r#"{"error":"echo poll panicked"}"#.into(),
        ))
    })
}

/// `cancel(handle)`. Idempotent + thread-safe + does NOT free. Echo has nothing
/// to wait for; the next poll observes its cancellation flag.
extern "C" fn echo_cancel(handle: StepHandle) {
    ECHO_DRIVES.cancel(handle);
}

/// `destroy(handle)`. Retires the token. Null or repeated calls are no-ops. Called
/// exactly once by the host's blocking driver.
extern "C" fn echo_destroy(handle: StepHandle) {
    drop(ECHO_DRIVES.remove(handle));
}

// ---------------------------------------------------------------------------
// The event handler — proves the on() dispatch path is wired (B2 minimum).
// ---------------------------------------------------------------------------

/// How many `MessageEnd` events the handler has received. Read from the host
/// (rpi-cli's smoke test asserts it fires when a run produces an assistant
/// message). A process-global atomic because `extern "C" fn` cannot capture.
static MESSAGE_END_HITS: AtomicUsize = AtomicUsize::new(0);

/// The handler the plugin registers for `MessageEnd`. Receives a
/// [`rpi_plugin_sdk::StablePluginEvent`]; per the SDK contract the handler MUST
/// NOT free the event's strings (the host frees exactly once after the fan-out).
/// Returns 0 (success); nonzero would be logged but not abort the fan-out.
extern "C" fn on_message_end(
    _event: rpi_plugin_sdk::StablePluginEvent,
    _user_data: *mut c_void,
) -> i32 {
    // Contain panics: returning nonzero is logged by the host (it never aborts
    // the fan-out), whereas unwinding would abort the host process.
    rpi_plugin_sdk::guard_or(1, || {
        MESSAGE_END_HITS.fetch_add(1, Ordering::SeqCst);
        0
    })
}

/// Read the `MessageEnd` hit counter (host smoke test uses this). Returns the
/// number of times the handler has fired in this process.
#[no_mangle]
pub extern "C" fn plugin_stub_message_end_hits() -> usize {
    MESSAGE_END_HITS.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// The resources_discover handler (B5b) — proves the discover round-trip
// ---------------------------------------------------------------------------

/// A canned skill path the discover handler advertises. The host merges this
/// into its skill-loading list (alongside static `.pi/skills` dirs) and the path
/// flows into the system prompt's `<available_skills>` block. The path points
/// at a `SKILL.md` shipped alongside the cdylib in the build dir — the host
/// smoke test asserts it appears in the discover result.
///
/// `discovered_echo` lives as a pre-canned skill the plugin "discovers" so the
/// round-trip has something concrete to assert without the plugin needing write
/// access at runtime. The fn below is `#[no_mangle]` so the host test can read
/// the exact path it expects back (mirrors `plugin_stub_message_end_hits`).
#[no_mangle]
pub extern "C" fn plugin_stub_discover_skill_path() -> *const u8 {
    DISCOVER_SKILL.as_ptr()
}

/// How many times the discover handler has been invoked (host smoke asserts ≥1).
#[no_mangle]
pub extern "C" fn plugin_stub_discover_hits() -> usize {
    DISCOVER_HITS.load(Ordering::SeqCst)
}

/// The canned skill path this stub advertises via `resources_discover`. A
/// `static` so the handler can hand a pointer back without allocating (and
/// `plugin_stub_discover_skill_path` returns its head). The path is a fixed
/// string the test resolves against the cdylib's own directory at runtime —
/// see `plugin_stub_discover_skills_dir`.
static DISCOVER_SKILL: &[u8] = b"plugin-stub-discovered/SKILL.md";
static DISCOVER_HITS: AtomicUsize = AtomicUsize::new(0);

rpi_plugin_sdk::string_handler! {
    /// `resources_discover(cwd, reason, out, user_data) -> i32`.
    ///
    /// Receives `cwd` + `reason` (`"startup"` or `"reload"`) as borrowed
    /// [`StbStringRef`]s and writes a JSON `{skillPaths:[...]}` payload into the
    /// `out` [`StbString`] (plugin-owned; the host reclaims it via the
    /// `plugin_free_string` we handed the host at registration). Returns `0`.
    ///
    /// The payload carries one skill path — a fixed marker string the host smoke
    /// test asserts appears in `emit_resources_discover`'s merged result. We model
    /// the real contract (a plugin hands back paths it discovered) without the
    /// plugin needing filesystem access at runtime.
    fn on_resources_discover((_cwd, _reason), out, _user_data) {
        DISCOVER_HITS.fetch_add(1, Ordering::SeqCst);
        // Build the JSON payload the host parses. `skillPaths` only (prompt/theme
        // omitted — lenient host treats missing fields as empty).
        let path = std::str::from_utf8(DISCOVER_SKILL).unwrap_or("");
        let json = format!(r#"{{"skillPaths":["{}"]}}"#, path);
        i32::from(!out.write(json))
    }
}

// ---------------------------------------------------------------------------
// The before_agent_start prompt transformer — proves the system-prompt round-trip
// ---------------------------------------------------------------------------

/// Appended to whatever prompt the host would otherwise use. A prefix marker so
/// the smoke test can assert the host applied *this plugin's* text to the real
/// request rather than something upstream already contained.
pub const STUB_PROMPT_MARKER: &str = "[plugin-stub says: ";
static PROMPT_TRANSFORM_HITS: AtomicUsize = AtomicUsize::new(0);

/// Exported so the smoke test can read the hit counter across the cdylib
/// boundary (the atomic lives in the plugin's address space).
#[no_mangle]
pub extern "C" fn plugin_stub_prompt_transform_hits() -> usize {
    PROMPT_TRANSFORM_HITS.load(Ordering::SeqCst)
}

rpi_plugin_sdk::string_handler! {
    /// `before_agent_start(event_json, out, user_data) -> i32`.
    ///
    /// Reads `systemPrompt` out of the borrowed event envelope and writes back
    /// `{"systemPrompt": "<marker><original>"}` into the plugin-owned `out` the host
    /// reclaims. This is deliberately an **append** — pi's contract makes the return
    /// value a replacement, so a plugin that wants to add text must read the current
    /// prompt and concatenate it, which is exactly what this does. A test asserts
    /// both that the marker is present and that the original text survived, so a
    /// host that treated the return as a wholesale replacement (dropping the base
    /// prompt) would fail.
    fn on_before_agent_start(event_json, out, _user_data) {
        PROMPT_TRANSFORM_HITS.fetch_add(1, Ordering::SeqCst);
        let current = serde_json::from_str::<serde_json::Value>(event_json)
            .ok()
            .and_then(|v| v.get("systemPrompt")?.as_str().map(str::to_string))
            .unwrap_or_default();
        let json = serde_json::json!({ "systemPrompt": format!("{STUB_PROMPT_MARKER}{current}") });
        i32::from(!out.write(json.to_string()))
    }
}

// ---------------------------------------------------------------------------
// The register provider (B5c) — proves the provider-injection round-trip
// ---------------------------------------------------------------------------

/// How many times the stub's `ProviderRequestFn` was called. The host smoke test
/// reads this through the cdylib to assert the provider was driven. A
/// process-global atomic because `extern "C" fn` cannot capture.
static PROVIDER_REQUEST_HITS: AtomicUsize = AtomicUsize::new(0);

/// Read the `ProviderRequestFn` hit counter (host smoke test uses this).
#[no_mangle]
pub extern "C" fn plugin_stub_provider_request_hits() -> usize {
    PROVIDER_REQUEST_HITS.load(Ordering::SeqCst)
}

rpi_plugin_sdk::string_handler! {
    /// The stub's `ProviderRequestFn`: `req_json` is a borrowed request envelope
    /// (`{model, context, options}`); `out` is an owning response we write a full
    /// assistant-message JSON into (the host parses it + reclaims it via our
    /// `plugin_free_string`). v1 one-shot: the stub returns the complete message as
    /// one terminal payload (the host's `PluggableProvider` emits it as a single
    /// `Done` chunk).
    ///
    /// We return a canned text message so the round-trip has something concrete to
    /// assert (the model the host routes here is `stub-model` — see the smoke test).
    fn on_provider_request(_req_json, out, _user_data) {
        PROVIDER_REQUEST_HITS.fetch_add(1, Ordering::SeqCst);
        let json = r#"{"role":"assistant","content":[{"type":"text","text":"from-stub-provider"}],"api":"faux","provider":"stub-provider","model":"stub-model","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"stopReason":"stop","timestamp":0}"#;
        i32::from(!out.write(json.to_owned()))
    }
}

// ---------------------------------------------------------------------------
// The streaming provider (B5c-s) — proves the delta round-trip
// ---------------------------------------------------------------------------

/// How many chunks the stub's streaming request fn emitted. The host smoke test
/// reads this to assert the callback path ran (and that the plugin received a
/// usable `emit`).
static STREAM_EMIT_HITS: AtomicUsize = AtomicUsize::new(0);

/// Read the streaming-emit hit counter (host smoke test uses this).
#[no_mangle]
pub extern "C" fn plugin_stub_stream_emit_hits() -> usize {
    STREAM_EMIT_HITS.load(Ordering::SeqCst)
}

/// The stub's streaming `ProviderStreamRequestFn`.
///
/// A real provider runs its HTTP/SSE loop here and emits one chunk per wire
/// delta. The stub has no network, so it emits a fixed sequence through `emit`
/// (thinking + text, with the `start`/`*_end` and `usage` frames) and then writes
/// the matching final assistant-message JSON to `out`. That exercises every chunk
/// kind the host mapper understands, in order.
///
/// Ownership: each chunk is an SDK-owned `StbString` transferred to the host,
/// which reclaims it through the `plugin_free_string` we registered alongside.
/// `out` is the final message, likewise plugin-owned. `emit_user_data` is the
/// host's bridge pointer and must be passed back unchanged.
extern "C" fn on_provider_stream_request(
    req_json: StbStringRef,
    emit: Option<ProviderStreamEmitFn>,
    emit_user_data: *mut c_void,
    // Declared but deliberately unused here: this stub shows the
    // fire-and-forget shape, where a plugin streams its fixed output and the
    // host still terminates an aborted run. See
    // `on_cancellable_provider_stream_request` for the polling shape.
    _control: *const ProviderStreamControl,
    _completion: Option<ProviderStreamCompletionFn>,
    _completion_user_data: *mut c_void,
    out: *mut StbString,
    _user_data: *mut c_void,
) -> i32 {
    rpi_plugin_sdk::guard_or(1, || {
        // Parse the envelope so a real provider's first step is exercised.
        let request = unsafe { req_json.as_str() };
        if !request.contains("\"model\"") {
            return 2;
        }

        let chunks = [
            r#"{"type":"start"}"#,
            r#"{"type":"thinking_start","contentIndex":0}"#,
            r#"{"type":"thinking_delta","contentIndex":0,"delta":"weighing"}"#,
            r#"{"type":"thinking_delta","contentIndex":0,"delta":" options"}"#,
            r#"{"type":"thinking_end","contentIndex":0,"content":"weighing options"}"#,
            r#"{"type":"text_start","contentIndex":1}"#,
            r#"{"type":"text_delta","contentIndex":1,"delta":"from-stub"}"#,
            r#"{"type":"text_delta","contentIndex":1,"delta":"-stream"}"#,
            r#"{"type":"text_end","contentIndex":1,"content":"from-stub-stream"}"#,
            r#"{"type":"usage","usage":{"input":3,"output":4,"cacheRead":0,"cacheWrite":0,"totalTokens":7,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}}}"#,
        ];

        if let Some(emit) = emit {
            for chunk in chunks {
                let rc = emit(StbString::from_string(chunk.to_string()), emit_user_data);
                if rc != 0 {
                    // The host rejected a chunk (a cancelled run, say). Stop
                    // emitting but still deliver the final message so the run
                    // terminates cleanly instead of hanging.
                    break;
                }
                STREAM_EMIT_HITS.fetch_add(1, Ordering::SeqCst);
            }
        }

        let json = r#"{"role":"assistant","content":[{"type":"thinking","thinking":"weighing options"},{"type":"text","text":"from-stub-stream"}],"api":"faux","provider":"stub-streaming-provider","model":"stub-model","usage":{"input":3,"output":4,"cacheRead":0,"cacheWrite":0,"totalTokens":7,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"stopReason":"stop","timestamp":0}"#;
        // Raw ABI slot (not the `string_handler!` sugar): write the owning
        // output through the pointer. `out` is aligned + initialized per the
        // contract, and this fn is the sole writer for the call.
        unsafe {
            *out = StbString::from_string(json.to_string());
        }
        0
    })
}

// ---------------------------------------------------------------------------
// The cancellable streaming provider (B5c-s) — proves the cancel protocol
// ---------------------------------------------------------------------------

/// Chunks the cancellable provider managed to emit before it noticed the
/// cancellation. The smoke test asserts this stays **below** the number it would
/// have produced if it ignored the signal.
static CANCEL_EMITS_BEFORE_STOP: AtomicUsize = AtomicUsize::new(0);

/// Set once the cancellable provider observed `is_cancelled` returning true.
static CANCEL_OBSERVED: AtomicUsize = AtomicUsize::new(0);

/// Read the "emitted before stopping" counter.
#[no_mangle]
pub extern "C" fn plugin_stub_cancel_emits_before_stop() -> usize {
    CANCEL_EMITS_BEFORE_STOP.load(Ordering::SeqCst)
}

/// Read the "observed cancellation" flag (0/1).
#[no_mangle]
pub extern "C" fn plugin_stub_cancel_observed() -> usize {
    CANCEL_OBSERVED.load(Ordering::SeqCst)
}

/// Reset both counters so a repeated test run starts from a known state.
#[no_mangle]
pub extern "C" fn plugin_stub_reset_cancel_counters() {
    CANCEL_EMITS_BEFORE_STOP.store(0, Ordering::SeqCst);
    CANCEL_OBSERVED.store(0, Ordering::SeqCst);
}

/// How many deltas the cancellable provider tries to emit.
///
/// Deliberately large: if the plugin honours the cancellation signal it stops
/// after a couple of chunks, and the gap between this number and
/// `plugin_stub_cancel_emits_before_stop()` is the observable proof. The sleeps
/// give the host's `select!` a chance to observe the token, so the test is not
/// racing a plugin that finishes before the cancellation can land.
const CANCEL_TOTAL_DELTAS: usize = 200;

/// The stub's cancellable streaming request fn: same chunk protocol as
/// [`on_provider_stream_request`], but it actually **polls** the control surface.
///
/// This is the shape a real long-running provider should have — an SSE loop that
/// checks `control.is_cancelled` between chunks and stops as soon as the caller
/// has gone away, instead of streaming a whole generation nobody will read.
///
/// Note there is no separate "cancellable registration": both stubs register
/// through the same `register_streaming_provider` and receive the same
/// `control`. Honoring it is the plugin's choice.
///
/// `control` is `null` only against a host that cannot describe its control
/// surface; the stub then falls back to the emit-everything behavior rather than
/// refusing to run.
extern "C" fn on_cancellable_provider_stream_request(
    req_json: StbStringRef,
    emit: Option<ProviderStreamEmitFn>,
    emit_user_data: *mut c_void,
    control: *const ProviderStreamControl,
    completion: Option<ProviderStreamCompletionFn>,
    completion_user_data: *mut c_void,
    out: *mut StbString,
    _user_data: *mut c_void,
) -> i32 {
    rpi_plugin_sdk::guard_or(1, || {
        let request = unsafe { req_json.as_str() };
        if !request.contains("\"model\"") {
            return 2;
        }
        // This host offers no deferred-completion channel; a plugin must not
        // expect one (see `ProviderStreamCompletionFn`).
        let _ = (completion, completion_user_data);

        // SAFETY: the host keeps the control struct live for this call only, and
        // hands it over exactly once. Null against an older/limited host.
        let control = if control.is_null() {
            None
        } else {
            Some(unsafe { *control })
        };

        let cancelled = |kind: ProviderChunkKind| -> bool {
            match control {
                Some(c) => {
                    (c.is_cancelled)(c.user_data, kind as u32) != 0
                        || (c.wait_cancelled_ms)(c.user_data, 0) != 0
                }
                None => false,
            }
        };

        if let Some(emit) = emit {
            let _ = emit(
                StbString::from_string(r#"{"type":"text_start","contentIndex":0}"#.to_string()),
                emit_user_data,
            );
            for index in 0..CANCEL_TOTAL_DELTAS {
                // The check a real provider makes between SSE frames. Poll the
                // cheap flag first, then block briefly (a real plugin would be
                // parked in a socket read here).
                if cancelled(ProviderChunkKind::Content) {
                    CANCEL_OBSERVED.store(1, Ordering::SeqCst);
                    return 1;
                }
                if let Some(c) = control {
                    if (c.wait_cancelled_ms)(c.user_data, 20) != 0 {
                        CANCEL_OBSERVED.store(1, Ordering::SeqCst);
                        return 1;
                    }
                }
                let chunk =
                    format!(r#"{{"type":"text_delta","contentIndex":0,"delta":"{index},"}}"#);
                if emit(StbString::from_string(chunk), emit_user_data) != 0 {
                    // The host rejected a content chunk: it has stopped
                    // listening. Same conclusion as an explicit cancellation.
                    CANCEL_OBSERVED.store(1, Ordering::SeqCst);
                    CANCEL_EMITS_BEFORE_STOP.fetch_add(1, Ordering::SeqCst);
                    return 1;
                }
                CANCEL_EMITS_BEFORE_STOP.fetch_add(1, Ordering::SeqCst);
            }
        }

        let json = r#"{"role":"assistant","content":[{"type":"text","text":"cancel-provider-finished"}],"api":"faux","provider":"stub-cancel-provider","model":"stub-model","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"stopReason":"stop","timestamp":0}"#;
        // SAFETY: sole writer for this call; `out` is aligned + initialized.
        unsafe {
            *out = StbString::from_string(json.to_string());
        }
        0
    })
}

// ---------------------------------------------------------------------------
// The markdown-transformer renderer (B5c) — proves the renderer registrar
// ---------------------------------------------------------------------------

/// How many times the stub's markdown `RenderFn` was called. The host smoke test
/// reads this to assert registration landed (B5e wires the TUI consumption; for
/// now registration + exposure is the B5c smoke — calling it is a unit-level
/// check the smoke optionally performs).
static MARKDOWN_TRANSFORM_HITS: AtomicUsize = AtomicUsize::new(0);

/// Read the markdown-transformer hit counter (host smoke test uses this).
#[no_mangle]
pub extern "C" fn plugin_stub_markdown_transform_hits() -> usize {
    MARKDOWN_TRANSFORM_HITS.load(Ordering::SeqCst)
}

rpi_plugin_sdk::string_handler! {
    /// The stub's markdown `RenderFn`: `input_json` is a borrowed
    /// `{ "markdown": "..." }`; `out` gets the transformed markdown JSON
    /// `{ "markdown": "<uppercased>" }` (a trivial transform so the round-trip has a
    /// concrete assertion). The host reclaims `out` via our `plugin_free_string`.
    fn on_markdown_transform(input_json, out, _user_data) {
        MARKDOWN_TRANSFORM_HITS.fetch_add(1, Ordering::SeqCst);
        // Parse the input (borrowed — must NOT free). lenient: missing `markdown` ⇒ "".
        let parsed: serde_json::Value =
            serde_json::from_str(input_json).unwrap_or(serde_json::Value::Null);
        let md = parsed
            .get("markdown")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_uppercase();
        let json = format!(r#"{{"markdown":"{}"}}"#, escape_json_string(&md));
        i32::from(!out.write(json))
    }
}

// ---------------------------------------------------------------------------
// The register entrypoint
// ---------------------------------------------------------------------------

// The SDK macro exports the unified `rpi_plugin_register` symbol and applies
// the version/null checks before running our registration body. Return 0 on
// success; nonzero means the host logs and skips this plugin.
rpi_plugin_sdk::export_plugin!(|api| {
    // Opt-in real registration side effect for native host lifecycle tests.
    // The build-only digest proves a test loaded its freshly built source, not
    // an unrelated DLL in a shared target directory. No output by default.
    if let Some(path) = std::env::var_os("RPI_PLUGIN_STUB_REGISTRATION_LOG") {
        use std::io::Write;
        if let Ok(mut log) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(
                log,
                "register {}",
                option_env!("RPI_RESOURCE_FIXTURE_SOURCE_SHA256").unwrap_or("unversioned")
            );
        }
    }
    // Register the echo tool. Build an owning StableToolSchema (name +
    // description + parameters-as-JSON); the host frees the schema's strings
    // via our `plugin_free_string` after copying them out.
    let schema = Box::new(StableToolSchema {
        name: StbString::from_string("echo".to_string()),
        description: StbString::from_string(
            "Echoes back the provided text (a B2 ABI smoke-test tool).".to_string(),
        ),
        parameters: StbString::from_string(
            r#"{"type":"object","properties":{"text":{"type":"string","description":"The text to echo back."}},"required":["text"]}"#
                    .to_string(),
        ),
    });
    let schema_ptr = &*schema as *const StableToolSchema;

    let Some(register_tool) = api.register_tool else {
        // Host not yet wiring tool registration — refuse so the host logs a
        // diagnostic (should not happen against a B2+ host).
        return 1;
    };
    let rc = register_tool(
        schema_ptr,
        echo_execute,
        echo_poll,
        echo_cancel,
        echo_destroy,
        plugin_free_string,
    );
    // The host copied the schema strings out + freed them via
    // plugin_free_string inside the trampoline; we can drop our Box now
    // (the StbStrings inside were already freed — dropping the Box just
    // releases the container, the freed fields are Copy ptr+len, no double-free).
    drop(schema);
    if rc != 0 {
        return rc;
    }

    // Prompt contributions are opt-in and separate from the callable schema.
    // Older hosts lack this appended slot; echo still works without a summary.
    if let Some(register_tool_prompt) = api.register_tool_prompt {
        let rc = register_tool_prompt(
            StbStringRef::from_str("echo"),
            StbStringRef::from_str(
                r#"{"promptSnippet":"Echo the supplied text","promptGuidelines":["Use echo only when an echo is requested"]}"#,
            ),
        );
        if rc != 0 {
            return rc;
        }
    }

    // Register the MessageEnd event handler (the on() surface).
    let Some(register_event_handler) = api.register_event_handler else {
        return 2;
    };
    let rc = register_event_handler(EventTag::MessageEnd, on_message_end, std::ptr::null_mut());
    if rc != 0 {
        return rc;
    }

    // Register the resources_discover handler (B5b). The host stores our
    // handler + our `plugin_free_string` (the `out` StbString we produce is
    // plugin-owned; the host reclaims it via that fn) + our `user_data`. On
    // discovery the host fans the event out to every registered handler and
    // merges their returned `{skillPaths, promptPaths, themePaths}`.
    let Some(register_resources_discover) = api.register_resources_discover else {
        // Host without the discover path — degrade (the smoke test host
        // always wires it; a non-wiring host logs + we continue).
        return 3;
    };
    let rc = register_resources_discover(
        on_resources_discover,
        plugin_free_string,
        std::ptr::null_mut(),
    );
    if rc != 0 {
        return rc;
    }

    // Register the before_agent_start prompt transformer. The host stores our
    // handler + our `plugin_free_string` + our `user_data`, then calls it on
    // every provider request to let us replace the system prompt the request
    // carries (pi's `before_agent_start` returning `systemPrompt`). A host that
    // predates the appended slot leaves it `None`; we degrade and continue,
    // because the prompt transform is optional to this stub's other duties.
    if let Some(register_before_agent_start) = api.register_before_agent_start {
        let rc = register_before_agent_start(
            on_before_agent_start,
            plugin_free_string,
            std::ptr::null_mut(),
        );
        if rc != 0 {
            return rc;
        }
    }

    // Register a custom provider (B5c). The host wraps `on_provider_request`
    // in a `PluggableProvider` impl of `rpi_ai::Provider` and injects it into
    // `AgentHarnessOptions.models`; a catalog model whose `provider` field is
    // `"stub-provider"` routes to it. The host calls `request_fn` on
    // `spawn_blocking` (it's sync), reclaims the plugin-owned `out` via our
    // `plugin_free_string`, parses it as an assistant message, emits it as one
    // terminal chunk.
    let Some(register_provider) = api.register_provider else {
        // Host without the provider path — degrade.
        return 4;
    };
    let rc = register_provider(
        StbStringRef::from_str("stub-provider"),
        StbStringRef::from_str("https://stub.example"),
        StbStringRef::from_str("faux"),
        on_provider_request,
        plugin_free_string,
        std::ptr::null_mut(),
    );
    if rc != 0 {
        return rc;
    }

    // Register a **streaming** provider (B5c-s). Same identity triple as above
    // but a different id, plus the per-chunk callback the host relays into
    // `AssistantMessageEvent`s while the request is still running. This slot is
    // nullable: a host that predates it falls back to the one-shot provider
    // above, which is exactly why the stub registers both.
    if let Some(register_streaming_provider) = api.register_streaming_provider {
        let rc = register_streaming_provider(
            StbStringRef::from_str("stub-streaming-provider"),
            StbStringRef::from_str("https://stub.example"),
            StbStringRef::from_str("faux"),
            on_provider_stream_request,
            plugin_free_string,
            std::ptr::null_mut(),
        );
        if rc != 0 {
            return rc;
        }

        // A second streaming provider that actually **uses** the cancellation
        // control surface: it emits on a budget and stops as soon as the caller
        // goes away. Registration is identical — one method, and the caller
        // decides whether to poll `control`.
        let rc = register_streaming_provider(
            StbStringRef::from_str("stub-cancel-provider"),
            StbStringRef::from_str("https://stub.example"),
            StbStringRef::from_str("faux"),
            on_cancellable_provider_stream_request,
            plugin_free_string,
            std::ptr::null_mut(),
        );
        if rc != 0 {
            return rc;
        }
    }

    // Register a markdown transformer (B5c). The host records it now; TUI
    // consumption is B5e (the render path calls `RenderFn` at display time).
    // We register one to prove the registrar + registry storage + snapshot
    // exposure round-trips.
    let Some(register_markdown_transformer) = api.register_markdown_transformer else {
        return 5;
    };
    let rc = register_markdown_transformer(
        StbStringRef::from_str("stub-uppercase"),
        on_markdown_transform,
        plugin_free_string,
        std::ptr::null_mut(),
    );
    if rc != 0 {
        return rc;
    }

    0
});

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Escape a `&str` for safe embedding inside a JSON string literal. We hand-build
/// the echo result JSON (above) rather than depend on serde in the cdylib's hot
/// path, so we must escape the user text ourselves.
fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// A compile-time assertion that StepResultTag is the repr we expect — guards a
// silent layout drift in the SDK that would corrupt the poll() return.
#[allow(dead_code)]
const _ASSERT_STEP_TAG_REPR: () = assert!(
    StepResultTag::Pending as u32 == 0
        && StepResultTag::Done as u32 == 1
        && StepResultTag::Err as u32 == 2
);

#[cfg(test)]
mod echo_lifecycle_tests {
    use super::*;
    use rpi_plugin_sdk::callbacks::Step;

    fn execute() -> StepHandle {
        echo_execute(
            StbStringRef::from_str("test-call"),
            StbString::from_string(r#"{"text":"hello"}"#.into()),
            Some(plugin_free_string),
        )
    }

    fn poll(handle: StepHandle) -> Step {
        // SAFETY: echo_poll transfers a result allocated by this SDK instance.
        unsafe { rpi_plugin_sdk::take_step_result!(echo_poll(handle, None, std::ptr::null_mut())) }
    }

    #[test]
    fn echo_still_reports_pending_then_done() {
        let handle = execute();
        assert!(!handle.is_null());
        assert_eq!(poll(handle), Step::Pending(String::new()));
        let Step::Done(json) = poll(handle) else {
            panic!("expected done")
        };
        let result: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(result["content"][0]["text"], "echo: hello");
        echo_destroy(handle);
    }

    #[test]
    fn null_invalid_and_retired_handles_are_rejected_without_dereferencing() {
        let handle = execute();
        echo_destroy(handle);
        echo_destroy(handle);
        echo_destroy(std::ptr::null_mut());
        for invalid in [std::ptr::null_mut(), usize::MAX as StepHandle, handle] {
            let Step::Err(json) = poll(invalid) else {
                panic!("expected invalid handle")
            };
            assert!(json.contains("invalid or destroyed"));
        }
        let next = execute();
        assert_ne!(next, handle);
        echo_destroy(next);
    }

    #[test]
    fn cancellation_is_idempotent_and_does_not_free_the_state() {
        let handle = execute();
        echo_cancel(handle);
        echo_cancel(handle);
        assert!(ECHO_DRIVES.get(handle).is_some());
        let Step::Err(json) = poll(handle) else {
            panic!("expected cancellation")
        };
        assert!(json.contains("cancelled"));
        echo_destroy(handle);
        assert!(ECHO_DRIVES.get(handle).is_none());
    }

    #[test]
    fn string_callbacks_report_missing_output_without_writing_through_null() {
        let input = StbStringRef::from_str("{}");
        let slot = std::ptr::null_mut();
        let data = std::ptr::null_mut();
        assert_eq!(on_resources_discover(input, input, slot, data), 1);
        assert_eq!(on_before_agent_start(input, slot, data), 1);
        assert_eq!(on_provider_request(input, slot, data), 1);
        assert_eq!(on_markdown_transform(input, slot, data), 1);
    }
}
