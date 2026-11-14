//! End-to-end Part B2 smoke test: load the **real** `plugin-stub` cdylib
//! (`examples/plugin-stub`, built as a `.dll`/`.so`/`.dylib`) through the
//! production `load_session` path and drive its `echo` tool through the
//! `PluginToolAdapter` spawn_blocking bridge.
//!
//! This is the test the plan's verification §2 asks for: "adapter round-trips
//! a stub tool via the spawn_blocking bridge" — but against the **actual cdylib**
//! (not the in-process `extern "C"` stubs in `tool::tests`), so it exercises the
//! full `libloading` → `rpi_plugin_register_v2` → `register_tool` trampoline →
//! keepalive → adapter path.
//!
//! ## Locating the cdylib
//!
//! The test binary lives under `<target>/debug/deps/` (or `release/deps/`), so
//! `current_exe()` → walk up two levels → `<target>/<profile>/` is where
//! `plugin_stub.{dll,so,dylib}` lands. We probe that dir (and the `deps/`
//! subdir as a fallback) and **skip** the test (not fail) if the cdylib isn't
//! there — building `plugin-stub` is an opt-in step (`cargo build -p
//! plugin-stub`), and we don't want a workspace `cargo test` to flip red just
//! because the example wasn't compiled.
//!
//! ## What's asserted
//!
//! 1. `load_session` loads exactly one plugin + one tool named `echo`.
//! 2. The `echo` adapter, driven via `AgentTool::execute`, returns
//!    `echo: <text>` and the 4-fn lifecycle completes (destroy fires — we can't
//!    observe destroy directly across the cdylib, but a clean return with no
//!    hang after the select! loop is the proof; the in-process tests assert the
//!    destroy-count invariant).
//! 3. A `MessageEnd` handler is registered in the snapshot (B3 will fire it;
//!    B2 only proves registration landed).
//! 4. (B5b) A `resources_discover` handler is registered, and
//!    `emit_resources_discover` fans the event to it → its canned skill path
//!    appears in the merged result, and the handler's process-global hit counter
//!    bumped. This is the plan's verification §3 B5 smoke: "plugin-stub
//!    registers a `resources_discover` handler → its skill path appears …".
//! 5. (B5c) A custom provider is registered (`stub-provider`), and the stub's
//!    sync `ProviderRequestFn` — wrapped by `PluggableProvider` — drives and
//!    returns a full assistant-message payload through the event stream, with
//!    the `out` StbString reclaimed via the plugin's `free_string`. A markdown
//!    transformer is also registered (the registrar round-trips; calling it is a
//!    TUI concern, B5e).
//!
//! Run it after building the stub:
//! ```sh
//! cargo build -p plugin-stub && cargo test -p rpi-extensions --test plugin_stub_smoke
//! ```

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Arc;

use rpi_agent::agent_tool::AgentTool;
use rpi_agent::types::{TextContentOrImage, ToolResultPartial};
use rpi_extensions::{load_session, PluginDiagnostics, PluginToolAdapter};
use rpi_plugin_sdk::EventTag;
use tokio_util::sync::CancellationToken;

/// Locate the built `plugin_stub` cdylib by walking up from the test binary's
/// own location (`<target>/<profile>/deps/`). Returns the cdylib path and the
/// dir it sits in (the dir is what `load_session` scans).
fn locate_stub() -> Option<(PathBuf, PathBuf)> {
    let exe = std::env::current_exe().ok()?;
    // exe ≈ <target>/debug/deps/rpi_extensions-<hash>.exe
    let deps = exe.parent()?; // .../deps
    let profile_dir = deps.parent()?; // <target>/debug  (or release)
    let cdylib = find_cdylib(profile_dir).or_else(|| find_cdylib(deps))?;
    let dir = cdylib.parent()?.to_path_buf();
    Some((cdylib, dir))
}

/// The cdylib filename per platform.
fn cdylib_filename() -> &'static str {
    if cfg!(windows) {
        "plugin_stub.dll"
    } else if cfg!(target_os = "macos") {
        "libplugin_stub.dylib"
    } else {
        "libplugin_stub.so"
    }
}

fn find_cdylib(dir: &std::path::Path) -> Option<PathBuf> {
    let p = dir.join(cdylib_filename());
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

/// A diagnostics sink that records warnings so the test can assert e.g. "no
/// ABI-mismatch skip happened".
#[derive(Default)]
struct RecordingDiag {
    warnings: std::sync::Mutex<Vec<String>>,
}
impl PluginDiagnostics for RecordingDiag {
    fn warn(&self, message: &str) {
        self.warnings.lock().unwrap().push(message.to_string());
    }
    fn unsupported(&self, _message: &str) {}
}

#[tokio::test]
async fn loads_real_cdylib_and_drives_echo_tool() {
    let Some((stub_src, _)) = locate_stub() else {
        eprintln!("plugin_stub cdylib not built — skipping (run `cargo build -p plugin-stub`)");
        return;
    };
    // Isolate the scan: copy the stub into a fresh temp dir so `load_session`
    // sees ONLY our cdylib. A shared `CARGO_TARGET_DIR` (e.g. under CI or a
    // user's global cargo target) can contain unrelated cdylibs from other
    // projects; the dir scan would diagnose those as load skips and trip the
    // `warnings.is_empty()` assertion below. The copy is also the path used for
    // the process-global hit-counter reads (`stub_*_hits`), so both `load_session`
    // and the test must load the SAME file (same module → shared statics).
    let scratch = tempfile::tempdir().expect("create temp dir for stub isolation");
    let stub_path = scratch.path().join(cdylib_filename());
    std::fs::copy(&stub_src, &stub_path).expect("copy stub cdylib into temp dir");
    let dir = scratch.path().to_path_buf();
    eprintln!("smoke: loading {}", stub_path.display());

    let diag = Arc::new(RecordingDiag::default());
    let warnings = Arc::clone(&diag);
    let session = load_session(
        &[dir],
        Arc::clone(&diag) as Arc<dyn PluginDiagnostics>,
        None,
    );

    // No load warnings (ABI mismatch / skip would land here).
    let warned = warnings.warnings.lock().unwrap().clone();
    assert!(warned.is_empty(), "unexpected load diagnostics: {warned:?}");
    assert!(!session.is_empty(), "expected the stub to load");
    assert_eq!(session.loaded_paths().len(), 1);

    let snapshot = session.snapshot().expect("snapshot present after a load");

    // One tool registered: echo.
    let tools = snapshot.tools();
    assert_eq!(tools.len(), 1, "echo should be the only registered tool");
    let echo = &tools[0];
    assert_eq!(echo.tool.name, "echo");
    assert_eq!(
        echo.prompt.prompt_snippet.as_deref(),
        Some("Echo the supplied text")
    );
    assert_eq!(
        echo.prompt.prompt_guidelines,
        ["Use echo only when an echo is requested"]
    );

    // One event handler registered on MessageEnd (B2 minimum on() proof).
    let handlers = snapshot.handlers_for(EventTag::MessageEnd);
    assert_eq!(
        handlers.len(),
        1,
        "expected one MessageEnd handler registered"
    );

    // Drive the echo tool through the real adapter (spawn_blocking bridge over
    // the cdylib's fn pointers — the keepalive keeps the dll mapped).
    let adapter = PluginToolAdapter::new(echo.tool.clone(), echo.handle(), session.keepalive());
    let on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync> = Arc::new(|_| {});
    let signal = CancellationToken::new();
    let result = adapter
        .execute(
            "smoke_call_1",
            serde_json::json!({ "text": "hi" }),
            signal,
            on_update,
        )
        .await
        .expect("echo execute should succeed");

    // The stub returns `echo: hi` as a single text block.
    assert_eq!(result.content.len(), 1, "result: {:?}", result.content);
    match &result.content[0] {
        TextContentOrImage::Text(t) => assert_eq!(t.text, "echo: hi"),
        other => panic!("expected text content, got {other:?}"),
    }
    assert!(!result.terminate, "echo should not terminate the session");

    // A second drive proves the adapter is reusable (destroy fired on the first
    // drive's handle; a fresh execute allocates a fresh handle).
    let on_update2: Arc<dyn Fn(ToolResultPartial) + Send + Sync> = Arc::new(|_| {});
    let signal2 = CancellationToken::new();
    let _user: *mut c_void = std::ptr::null_mut();
    let _ = _user;
    let result2 = adapter
        .execute(
            "smoke_call_2",
            serde_json::json!({ "text": "again" }),
            signal2,
            on_update2,
        )
        .await
        .expect("second echo execute should succeed");
    match &result2.content[0] {
        TextContentOrImage::Text(t) => assert_eq!(t.text, "echo: again"),
        other => panic!("expected text content, got {other:?}"),
    }

    // ---- B3a: the ExtensionEmitter round-trips AgentEvent → handlers --------
    // The stub registered one `MessageEnd` handler that bumps a process-global
    // counter. Driving the emitter with a synthetic MessageEnd event proves the
    // full dispatch path (translate → catch_unwind fan-out → plugin handler) is
    // wired against the real cdylib. We read the counter back via the cdylib's
    // exported `plugin_stub_message_end_hits` accessor (looked up the same way
    // the loader looks up `rpi_plugin_register_v2`).
    use rpi_agent::events::AgentEmitter;
    use rpi_agent::message::AgentMessage;
    use rpi_ai::types::{AssistantMessage, Content, Usage};
    use rpi_extensions::ExtensionEmitter;

    let snapshot = session.snapshot_arc().expect("snapshot present");
    let emitter = ExtensionEmitter::new(snapshot, session.keepalive());
    let am = AgentMessage::Assistant(Box::new(AssistantMessage {
        role: rpi_ai::types::AssistantRole,
        content: vec![Content::text("smoke")],
        api: rpi_ai::types::Api::AnthropicMessages,
        provider: "anthropic".to_string(),
        model: "m".into(),
        response_model: None,
        response_id: None,
        usage: Usage::zero(),
        stop_reason: rpi_ai::types::StopReason::Stop,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    }));
    let before = stub_message_end_hits(&stub_path);
    emitter.try_emit(rpi_agent::AgentEvent::MessageEnd {
        message_id: None,
        message: am,
    });
    let after = stub_message_end_hits(&stub_path);
    assert_eq!(
        after,
        before + 1,
        "MessageEnd handler in the real cdylib should have fired once"
    );

    // The session's keepalive is still alive (adapter holds a clone), so the
    // cdylib stays mapped; dropping the adapter + session unloads it.

    // ---- B5b: resources_discover round-trip through the real cdylib ----------
    // The stub registered a `resources_discover` handler that returns a canned
    // skill path. `emit_resources_discover` fans the event to every registered
    // handler in registration order, merges their `{skillPaths, promptPaths,
    // themePaths}` arrays, and returns the concat. We assert:
    //   (a) exactly one discover handler registered;
    //   (b) the handler fired (its process-global counter bumped);
    //   (c) its canned skill path appears in the merged result.
    // This is the plan verification §3 B5 smoke ("plugin-stub registers a
    // resources_discover handler → its skill path appears …").
    use rpi_extensions::emit_resources_discover;

    let snap = session.snapshot_arc().expect("snapshot present");
    let handlers = snap.resources_discover();
    assert_eq!(
        handlers.len(),
        1,
        "expected exactly one resources_discover handler registered"
    );

    let discover_before = stub_discover_hits(&stub_path);
    let discovered = emit_resources_discover("/cwd", "startup", &snap);
    let discover_after = stub_discover_hits(&stub_path);
    assert_eq!(
        discover_after,
        discover_before + 1,
        "resources_discover handler in the real cdylib should have fired once"
    );
    // The stub advertises exactly one skill path (its marker string). It must
    // land in the merged `skill_paths`; prompt/theme stay empty (the stub
    // returns only skillPaths).
    assert_eq!(
        discovered.skill_paths.len(),
        1,
        "one skill path from one handler"
    );
    assert_eq!(
        discovered.skill_paths[0], "plugin-stub-discovered/SKILL.md",
        "the canned path the stub advertises must round-trip unchanged"
    );
    assert!(
        discovered.prompt_paths.is_empty(),
        "stub returns no promptPaths"
    );
    assert!(
        discovered.theme_paths.is_empty(),
        "stub returns no themePaths"
    );

    // ---- B5c: custom provider + markdown-transformer round-trip -------------
    // (a) The stub registered one custom provider (`stub-provider`) + one
    //     markdown transformer (`stub-uppercase`). Both must land in the
    //     registry snapshot — proving the widened vtable slots' trampolines
    //     recorded the registrations (plugin_free_string + user_data carried).
    // (b) Build `PluggableProvider`s from the session (one per registered
    //     provider) and drive `stream_simple` against a model whose `provider`
    //     matches the stub's id: the stub's sync `ProviderRequestFn` runs on
    //     `spawn_blocking`, the plugin-owned `out` is reclaimed, the response
    //     JSON parses to an AssistantMessage, and it surfaces as one terminal
    //     `Done` chunk on the event stream — with the stub's hit counter bumped.
    // (c) Drive the markdown transformer's `RenderFn` directly through the
    //     snapshot record (B5e wires the TUI render path; this proves the fn
    //     pointer + free_string round-trip now).
    use rpi_ai::{Api, Model, Provider, SimpleStreamOptions};
    use rpi_plugin_sdk::StbStringRef;
    let snap = session.snapshot_arc().expect("snapshot present");

    // (a) registration landed.
    let providers = snap.providers();
    assert_eq!(
        providers.len(),
        3,
        "the stub registers one one-shot and two streaming providers"
    );
    assert_eq!(providers[0].provider_id, "stub-provider");
    assert!(
        providers[0].stream_fn.is_none(),
        "the one-shot record must not claim a streaming fn"
    );
    let markdown_renderers = snap.renderers_of(rpi_extensions::RegisteredRendererKind::Markdown);
    assert_eq!(
        markdown_renderers.len(),
        1,
        "expected one markdown transformer registered"
    );
    assert_eq!(markdown_renderers[0].name, "stub-uppercase");

    // (b) PluggableProvider drives the stub's request_fn on a runtime. The test
    //     is `#[tokio::test]`, so the ambient runtime handle is available and
    //     `stream_simple` (which spawns its producer task on the captured
    //     handle) cooperates directly — no second runtime / `block_on`.
    let ambient = tokio::runtime::Handle::current();
    let pluggable = rpi_extensions::PluggableProvider::from_session(&session, ambient.clone());
    assert_eq!(
        pluggable.len(),
        3,
        "one PluggableProvider per registered provider"
    );
    let provider: Arc<dyn Provider> = pluggable
        .into_iter()
        .find(|p| p.id() == "stub-provider")
        .expect("the one-shot provider");
    assert_eq!(provider.id(), "stub-provider");

    let model = Model::new(
        "stub-model",
        "Stub",
        Api::Faux,
        "stub-provider",
        "https://stub.example",
    );
    let ctx = rpi_ai::Context::new(Vec::new());
    let opts = SimpleStreamOptions::default();

    let before = stub_provider_request_hits(&stub_path);
    // `stream_simple` is async — the producer task spawns on `ambient`. Await
    // the stream + drain the single terminal chunk (v1 one-shot).
    let mut ev_stream = provider.stream_simple(&model, &ctx, &opts).await;
    let mut terminal: Option<rpi_ai::AssistantMessageEvent> = None;
    while let Some(ev) = ev_stream.next().await {
        terminal = Some(ev);
    }
    let (reason, text, message_provider) = match terminal.expect("a terminal event") {
        rpi_ai::AssistantMessageEvent::Done { reason, message } => {
            let text = match &message.content[0] {
                rpi_ai::types::Content::Text(t) => t.text.clone(),
                other => panic!("expected text content, got {other:?}"),
            };
            (reason, text, message.provider)
        }
        other => panic!("expected Done terminal, got {other:?}"),
    };
    let after = stub_provider_request_hits(&stub_path);
    assert_eq!(
        after,
        before + 1,
        "the stub's ProviderRequestFn should have run once via spawn_blocking"
    );
    assert_eq!(reason, rpi_ai::types::DoneReason::Stop);
    assert_eq!(text, "from-stub-provider");
    assert_eq!(message_provider, "stub-provider");

    // (c) markdown transformer round-trip (call the RenderFn directly via the
    // recorded fn pointer + reclaim the out via the plugin's free_string).
    let renderer = &markdown_renderers[0];
    let input = r#"{"markdown":"hello world"}"#;
    let mut out = rpi_plugin_sdk::StbString::empty();
    let rc = (renderer.render_fn)(
        StbStringRef::from_str(input),
        &mut out as *mut rpi_plugin_sdk::StbString,
        renderer.user_data,
    );
    assert_eq!(rc, 0, "render_fn should succeed");
    // SAFETY: the ABI callback returned a live output, copied before producer-side cleanup.
    let transformed = unsafe {
        rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(renderer.plugin_free_string))
    }
    .to_string_lossy();
    assert!(
        transformed.contains(r#""markdown":"HELLO WORLD""#),
        "stub uppercases markdown: got {transformed}"
    );
    let md_hits = stub_markdown_transform_hits(&stub_path);
    assert!(md_hits >= 1, "markdown transform fn should have fired");

    // ---- B5c-s: the streaming provider emits real deltas -------------------
    // (a) The stub registered a second, **streaming** provider
    //     (`stub-streaming-provider`) whose record carries a `stream_fn`.
    // (b) Driving `stream_simple` against a model routed to it must yield the
    //     plugin's thinking and text before terminal `Done`, preserving block
    //     order and every character. Already-queued adjacent deltas may share an
    //     event; a lone available delta must still render during generation.
    // (c) The per-chunk callback is only reachable if the host handed the plugin
    //     a live bridge pointer, and every chunk the plugin produced must be
    //     reclaimed by the plugin's own `free_string` (a wrong deallocator would
    //     corrupt the plugin's heap and show up as a crash here).
    let streaming = snap
        .providers()
        .iter()
        .find(|p| p.provider_id == "stub-streaming-provider")
        .cloned()
        .expect("the stub registers a streaming provider");
    assert!(
        streaming.stream_fn.is_some(),
        "register_streaming_provider must record a stream_fn"
    );

    let streaming_provider: Arc<dyn Provider> =
        rpi_extensions::PluggableProvider::from_session(&session, ambient.clone())
            .into_iter()
            .find(|p| p.id() == "stub-streaming-provider")
            .expect("a PluggableProvider per registered provider, including the streaming one");

    let model = Model::new(
        "stub-model",
        "Stub",
        Api::Faux,
        "stub-streaming-provider",
        "https://stub.example",
    );
    let before_emits = stub_stream_emit_hits(&stub_path);
    let mut stream = streaming_provider.stream_simple(&model, &ctx, &opts).await;

    let mut tags: Vec<&'static str> = Vec::new();
    let mut thinking = String::new();
    let mut text = String::new();
    let mut terminal_reason = None;
    while let Some(event) = stream.next().await {
        tags.push(event.type_tag());
        match &event {
            rpi_ai::AssistantMessageEvent::ThinkingDelta { delta, .. } => thinking.push_str(delta),
            rpi_ai::AssistantMessageEvent::TextDelta { delta, .. } => text.push_str(delta),
            rpi_ai::AssistantMessageEvent::Done { reason, .. } => {
                terminal_reason = Some(*reason);
            }
            _ => {}
        }
    }

    assert_eq!(
        terminal_reason,
        Some(rpi_ai::types::DoneReason::Stop),
        "the streaming provider ends on a Done terminal"
    );
    assert_eq!(
        tags.first().copied(),
        Some("start"),
        "the host opens the stream before any delta: got {tags:?}"
    );
    assert_eq!(tags.last().copied(), Some("done"), "got {tags:?}");
    assert_eq!(
        thinking, "weighing options",
        "thinking text is lossless: got {tags:?}"
    );
    assert_eq!(
        text, "from-stub-stream",
        "response text is lossless: got {tags:?}"
    );
    let structure: Vec<_> = tags
        .iter()
        .copied()
        .filter(|tag| !matches!(*tag, "thinking_delta" | "text_delta"))
        .collect();
    assert_eq!(
        structure,
        [
            "start",
            "thinking_start",
            "thinking_end",
            "text_start",
            "text_end",
            "done"
        ]
    );
    // Order matters: the thinking deltas precede the text deltas, and no delta
    // arrives after the terminal event.
    let first_text = tags
        .iter()
        .position(|t| *t == "text_delta")
        .expect("a text delta");
    let last_thinking = tags
        .iter()
        .rposition(|t| *t == "thinking_delta")
        .expect("a thinking delta");
    assert!(
        last_thinking < first_text,
        "thinking is delivered before text: got {tags:?}"
    );
    assert_eq!(
        tags.iter().filter(|t| **t == "done").count(),
        1,
        "exactly one terminal event: got {tags:?}"
    );

    let after_emits = stub_stream_emit_hits(&stub_path);
    assert!(
        after_emits > before_emits,
        "the plugin's emit callback must have been invoked through the live bridge"
    );

    // The one-shot provider must be untouched by the streaming path: its hit
    // counter still reflects its own single call.
    assert_eq!(
        stub_provider_request_hits(&stub_path),
        after,
        "driving the streaming provider must not call the one-shot request_fn"
    );

    // ---- B5c-s: cancellation stops a plugin mid-generation ---------------
    // Both streaming providers register through the **same** method; the difference
    // is that `stub-cancel-provider` polls the control surface. It would otherwise
    // emit 200 deltas. Cancelling the run's token must make it **notice** and stop
    // early, and the caller must see an `Error { reason: Aborted }` terminal
    // promptly instead of waiting out a full generation. This is the property the
    // cancel protocol exists for.
    let cancelling = rpi_extensions::PluggableProvider::from_session(&session, ambient.clone())
        .into_iter()
        .find(|p| p.id() == "stub-cancel-provider")
        .expect("the stub registers a cancellable provider");

    let cancel_model = Model::new(
        "stub-model",
        "Stub",
        Api::Faux,
        "stub-cancel-provider",
        "https://stub.example",
    );
    // Fresh counters, so this test cannot be confused by the streaming run above.
    stub_reset_cancel_counters(&stub_path);

    let signal = tokio_util::sync::CancellationToken::new();
    let cancel_opts = SimpleStreamOptions {
        signal: signal.clone(),
        ..SimpleStreamOptions::default()
    };
    let mut cancel_stream = cancelling
        .stream_simple(&cancel_model, &ctx, &cancel_opts)
        .await;

    // Cancel as soon as the plugin has produced something — the point is to stop
    // it mid-stream, not before it starts.
    let mut seen_delta = false;
    let mut cancel_tags: Vec<&'static str> = Vec::new();
    let mut aborted = false;
    let cancel_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while let Some(event) = cancel_stream.next().await {
        cancel_tags.push(event.type_tag());
        match &event {
            rpi_ai::AssistantMessageEvent::TextDelta { .. } if !seen_delta => {
                seen_delta = true;
                signal.cancel();
            }
            rpi_ai::AssistantMessageEvent::Error { reason, .. } => {
                aborted = *reason == rpi_ai::types::ErrorReason::Aborted;
            }
            _ => {}
        }
        assert!(
            std::time::Instant::now() < cancel_deadline,
            "a cancelled plugin must terminate promptly, got {cancel_tags:?}"
        );
    }

    assert!(
        seen_delta,
        "the plugin must have started producing: {cancel_tags:?}"
    );
    assert!(
        aborted,
        "a cancelled run ends on Error {{ reason: Aborted }}: {cancel_tags:?}"
    );
    assert_eq!(
        cancel_tags.iter().filter(|t| **t == "error").count(),
        1,
        "exactly one terminal event: {cancel_tags:?}"
    );
    // The aborted terminal is delivered before the blocking plugin must finish.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while stub_cancel_observed(&stub_path) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the plugin eventually observes the cancellation latch");
    assert!(
        stub_cancel_observed(&stub_path) == 1,
        "the plugin must have observed the cancellation through its control surface"
    );
    let emitted = stub_cancel_emits_before_stop(&stub_path);
    assert!(
        emitted < 200,
        "the plugin must stop well before its 200-delta budget, emitted {emitted}"
    );
}

/// Read the stub's `plugin_stub_cancel_emits_before_stop` counter through the cdylib.
fn stub_cancel_emits_before_stop(stub_path: &std::path::Path) -> usize {
    stub_usize_counter(stub_path, b"plugin_stub_cancel_emits_before_stop\0")
}

/// Read the stub's `plugin_stub_cancel_observed` flag through the cdylib.
fn stub_cancel_observed(stub_path: &std::path::Path) -> usize {
    stub_usize_counter(stub_path, b"plugin_stub_cancel_observed\0")
}

/// Reset the stub's cancellation counters through the cdylib.
fn stub_reset_cancel_counters(stub_path: &std::path::Path) {
    let lib = match unsafe { libloading::Library::new(stub_path) } {
        Ok(l) => l,
        Err(_) => return,
    };
    type ResetFn = extern "C" fn();
    if let Ok(sym) = unsafe { lib.get::<ResetFn>(b"plugin_stub_reset_cancel_counters\0") } {
        sym();
    }
}

/// Read a `extern "C" fn() -> usize` counter exported by the stub cdylib.
fn stub_usize_counter(stub_path: &std::path::Path, symbol: &[u8]) -> usize {
    let lib = match unsafe { libloading::Library::new(stub_path) } {
        Ok(l) => l,
        Err(_) => return 0,
    };
    type HitFn = extern "C" fn() -> usize;
    let sym: libloading::Symbol<HitFn> = match unsafe { lib.get(symbol) } {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let hits = sym();
    drop(sym);
    drop(lib);
    hits
}

/// Read the stub's `plugin_stub_stream_emit_hits` counter through the cdylib.
fn stub_stream_emit_hits(stub_path: &std::path::Path) -> usize {
    let lib = match unsafe { libloading::Library::new(stub_path) } {
        Ok(l) => l,
        Err(_) => return 0,
    };
    type HitFn = extern "C" fn() -> usize;
    let sym: libloading::Symbol<HitFn> = match unsafe { lib.get(b"plugin_stub_stream_emit_hits\0") }
    {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let hits = sym();
    drop(sym);
    drop(lib);
    hits
}

/// Read the stub's `plugin_stub_prompt_transform_hits` counter through the cdylib.
fn stub_prompt_transform_hits(stub_path: &std::path::Path) -> usize {
    let lib = match unsafe { libloading::Library::new(stub_path) } {
        Ok(l) => l,
        Err(_) => return 0,
    };
    type HitFn = extern "C" fn() -> usize;
    let sym: libloading::Symbol<HitFn> =
        match unsafe { lib.get(b"plugin_stub_prompt_transform_hits\0") } {
            Ok(s) => s,
            Err(_) => return 0,
        };
    let hits = sym();
    drop(sym);
    drop(lib);
    hits
}

/// The system-prompt transform, driven end to end through the **real** cdylib.
///
/// This is the E2E proof that a plugin can change what the model is asked with:
/// the stub appends a marker to whatever prompt it is handed, and we assert the
/// host's `ProviderHooks::transform_context` seam applied it — including that the
/// base prompt survived, so the return value is a replacement the plugin built
/// from the prompt it was given (pi's contract) rather than a wholesale override
/// that dropped the original.
///
/// Also asserts the per-prompt caching: one prompt must run the handlers once
/// even across several provider calls, which is what stops the transform from
/// re-firing mid-run (pi computes it once and reuses it for the run).
#[tokio::test]
async fn a_real_cdylib_can_replace_the_system_prompt() {
    use rpi_ai::{Context, ProviderHooks};

    let Some((stub_src, _)) = locate_stub() else {
        eprintln!("plugin_stub cdylib not built — skipping (run `cargo build -p plugin-stub`)");
        return;
    };
    let scratch = tempfile::tempdir().expect("temp dir");
    let stub_path = scratch.path().join(cdylib_filename());
    std::fs::copy(&stub_src, &stub_path).expect("copy stub cdylib");

    let diag = Arc::new(RecordingDiag::default());
    let session = load_session(
        &[scratch.path().to_path_buf()],
        Arc::clone(&diag) as Arc<dyn PluginDiagnostics>,
        None,
    );
    // The bridge must exist even though this plugin registers no provider-hook
    // *event* handler for the prompt path — the transformer is its own slot. A
    // `from_session` that only looked at event tags would return `None` here and
    // silently ignore the transform.
    let hooks =
        rpi_extensions::ExtensionProviderHooks::from_session(&session).expect("bridge installed");

    let snapshot = session.snapshot().expect("snapshot");
    assert_eq!(
        snapshot.before_agent_start().len(),
        1,
        "the stub registers exactly one prompt transformer"
    );

    let model = rpi_ai::Model::new(
        "stub-model",
        "stub-model",
        rpi_ai::Api::AnthropicMessages,
        "stub-provider",
        "https://stub.example",
    );
    let ctx = Context::new(vec![rpi_ai::types::Message::User(
        rpi_ai::types::UserMessage::new(
            rpi_ai::types::UserContent::Text("do the thing".to_string()),
            1,
        ),
    )]);
    // The base prompt the plugin will append to.
    let ctx = Context {
        system_prompt: Some("BASE PROMPT".to_string()),
        ..ctx
    };

    let hits_before = stub_prompt_transform_hits(&stub_path);
    let patch = hooks
        .transform_context(&model, &ctx)
        .expect("the stub's transformer returns a patch");
    let hits_after = stub_prompt_transform_hits(&stub_path);

    assert_eq!(
        hits_after,
        hits_before + 1,
        "the real cdylib's transformer should have fired exactly once"
    );
    let applied = patch.system_prompt.expect("patch carries a system prompt");
    assert!(
        applied.contains("[plugin-stub says: "),
        "the plugin's marker must reach the live context; got {applied:?}"
    );
    assert!(
        applied.contains("BASE PROMPT"),
        "the base prompt the plugin appended to must survive; got {applied:?}"
    );

    // Per-prompt caching: a second provider call for the SAME prompt reuses the
    // computed result and does not re-run the handler.
    let again = hooks
        .transform_context(&model, &ctx)
        .expect("cached result is still a patch");
    assert_eq!(again.system_prompt.as_deref(), Some(applied.as_str()));
    assert_eq!(
        stub_prompt_transform_hits(&stub_path),
        hits_before + 1,
        "a second request for the same prompt must reuse the cached transform"
    );
}

/// Read the stub's `plugin_stub_discover_hits` counter through the cdylib
/// (second mapping — refcounted, same pattern as `stub_message_end_hits`).
fn stub_discover_hits(stub_path: &std::path::Path) -> usize {
    let lib = match unsafe { libloading::Library::new(stub_path) } {
        Ok(l) => l,
        Err(_) => return 0,
    };
    type HitFn = extern "C" fn() -> usize;
    let sym: libloading::Symbol<HitFn> = match unsafe { lib.get(b"plugin_stub_discover_hits\0") } {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let hits = sym();
    drop(sym);
    drop(lib);
    hits
}

/// Read the stub's `plugin_stub_provider_request_hits` counter through the
/// cdylib (second mapping — same refcounted pattern as the other helpers).
/// Returns 0 if the symbol isn't found (defensive against a stale build).
fn stub_provider_request_hits(stub_path: &std::path::Path) -> usize {
    let lib = match unsafe { libloading::Library::new(stub_path) } {
        Ok(l) => l,
        Err(_) => return 0,
    };
    type HitFn = extern "C" fn() -> usize;
    let sym: libloading::Symbol<HitFn> =
        match unsafe { lib.get(b"plugin_stub_provider_request_hits\0") } {
            Ok(s) => s,
            Err(_) => return 0,
        };
    let hits = sym();
    drop(sym);
    drop(lib);
    hits
}

/// Read the stub's `plugin_stub_markdown_transform_hits` counter through the
/// cdylib (second mapping — same refcounted pattern as the other helpers).
/// Returns 0 if the symbol isn't found (defensive against a stale build).
fn stub_markdown_transform_hits(stub_path: &std::path::Path) -> usize {
    let lib = match unsafe { libloading::Library::new(stub_path) } {
        Ok(l) => l,
        Err(_) => return 0,
    };
    type HitFn = extern "C" fn() -> usize;
    let sym: libloading::Symbol<HitFn> =
        match unsafe { lib.get(b"plugin_stub_markdown_transform_hits\0") } {
            Ok(s) => s,
            Err(_) => return 0,
        };
    let hits = sym();
    drop(sym);
    drop(lib);
    hits
}

/// Read the stub's `plugin_stub_message_end_hits` counter through the cdylib.
/// Loads the library fresh (a second mapping — cdylibs are refcounted on Windows
/// / dlopen-refcounted on Unix, so a second open is fine and the first open in
/// the session's keepalive stays alive). Returns 0 if the symbol isn't found
/// (defensive; the stub exports it, but a stale build might not).
fn stub_message_end_hits(stub_path: &std::path::Path) -> usize {
    let lib = match unsafe { libloading::Library::new(stub_path) } {
        Ok(l) => l,
        Err(_) => return 0,
    };
    type HitFn = extern "C" fn() -> usize;
    let sym: libloading::Symbol<HitFn> = match unsafe { lib.get(b"plugin_stub_message_end_hits\0") }
    {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let hits = sym();
    drop(sym);
    drop(lib);
    hits
}
