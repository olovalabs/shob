//! End-to-end smoke test for the **push-to-talk** host plumbing, driven through
//! the real `rpi_voice` cdylib.
//!
//! ## Why this test exists
//!
//! Push-to-talk spans two repos and an FFI boundary:
//!
//! 1. the plugin calls `register_shortcut("space", …)` to claim the key,
//! 2. it subscribes an `Input` event handler,
//! 3. the host TUI routes key presses/releases to that handler, and
//! 4. the handler's return code decides whether the key is swallowed.
//!
//! Steps 1–2 happen inside the cdylib and are invisible to unit tests in either
//! crate — a `register_shortcut` trampoline regression (or a plugin that forgets
//! to subscribe `Input`) would ship silently. This test loads the **actual built
//! cdylib** through the production `load_session` path and asserts the host ends
//! up with both registrations, so the wiring is verified end to end.
//!
//! ## Locating the cdylib
//!
//! The test binary lives under `<target>/<profile>/deps/`, so `current_exe()` →
//! up two levels is where `rpi_voice.{dll,so,dylib}` lands. We **skip** (not
//! fail) when it isn't built — `rpi-voice` lives in the *other* repo
//! (`rpi-package`), so a plain `cargo test` here must not go red just because
//! that crate wasn't compiled.
//!
//! Build it first:
//! ```sh
//! # in ../rpi-package
//! cargo build -p rpi-voice --release --features local-stt
//! ```
//! The test uses `RPI_VOICE_PTT_KEY` to assert the key name is honored rather
//! than hard-coding `space`.

use std::path::PathBuf;
use std::sync::Arc;

use rpi_extensions::{load_session, PluginDiagnostics, RegistrySnapshot};
use rpi_plugin_sdk::EventTag;

/// Diagnostics sink that records warnings (an ABI skip would land here).
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

fn cdylib_filename() -> &'static str {
    if cfg!(windows) {
        "rpi_voice.dll"
    } else if cfg!(target_os = "macos") {
        "librpi_voice.dylib"
    } else {
        "librpi_voice.so"
    }
}

/// Locate the built `rpi_voice` cdylib by walking up from the test binary.
/// Also accepts the built artifact directly out of the sibling repo's target
/// dir, since `rpi-voice` is a different workspace.
fn locate_cdylib() -> Option<PathBuf> {
    let name = cdylib_filename();

    // 1. This workspace's own target dir (if rpi-voice was ever built here).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(deps) = exe.parent() {
            for dir in [deps.parent(), Some(deps)] {
                if let Some(dir) = dir {
                    let candidate = dir.join(name);
                    if candidate.exists() {
                        return Some(candidate);
                    }
                    // rpi-package builds into its own (or the shared) target dir.
                    for sub in ["release", "debug"] {
                        let candidate = dir.join(sub).join(name);
                        if candidate.exists() {
                            return Some(candidate);
                        }
                    }
                }
            }
        }
    }

    // 2. The sibling repo's target dir, resolved relative to this crate.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest.parent()?.parent()?; // <pi-rust>/crates/rpi-extensions -> <pi-rust>
    let sibling = repo_root.parent()?.join("rpi-package");
    for profile in ["release", "debug"] {
        let candidate = sibling.join("target").join(profile).join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn load_voice() -> Option<(Arc<RecordingDiag>, Arc<RegistrySnapshot>)> {
    let src = locate_cdylib()?;
    // Isolate the scan: copy into a temp dir so unrelated cdylibs in a shared
    // target dir never produce diagnostics that would trip our assertions.
    let scratch = tempfile::tempdir().ok()?;
    let path = scratch.path().join(cdylib_filename());
    std::fs::copy(&src, &path).ok()?;
    eprintln!("ptt smoke: loading {}", src.display());

    let diag = Arc::new(RecordingDiag::default());
    let session = load_session(
        &[scratch.path().to_path_buf()],
        Arc::clone(&diag) as Arc<dyn PluginDiagnostics>,
        None,
    );

    // Take a shared snapshot before dropping the session: the snapshot holds the
    // plugin's fn pointers, and `snapshot_arc` is an owned handle to the merged
    // registry (so the session — and thus the cdylib — must outlive it).
    let snapshot = session.snapshot_arc();
    // Leak the session deliberately: dropping it unmaps the cdylib while the
    // snapshot still points into it. The OS reclaims everything at process exit,
    // and this test loads exactly one plugin once.
    std::mem::forget(session);
    snapshot.map(|s| (diag, s))
}

#[test]
fn voice_registers_ptt_shortcut_and_input_handler() {
    let Some((diag, snapshot)) = load_voice() else {
        eprintln!(
            "rpi_voice cdylib not built — skipping \
             (build it with `cargo build -p rpi-voice --release` in ../rpi-package)"
        );
        return;
    };

    let warned = diag.warnings.lock().unwrap().clone();
    assert!(warned.is_empty(), "unexpected load diagnostics: {warned:?}");

    // The key the plugin claimed. Defaults to `space`.
    let key = std::env::var("RPI_VOICE_PTT_KEY")
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "space".to_string());

    // 1. The shortcut was recorded by the host's registry (this is the
    //    `register_shortcut` trampoline — previously a no-op stub).
    assert!(
        snapshot.has_shortcut(&key),
        "plugin did not claim `{key}`; shortcuts={:?}",
        snapshot.shortcuts()
    );
    let shortcut = snapshot
        .shortcuts()
        .iter()
        .find(|s| s.key == key)
        .expect("shortcut present");
    assert_eq!(shortcut.plugin, "rpi_voice");

    // 2. An `Input` handler is subscribed, which is how the plugin receives the
    //    press/release events the TUI routes for a claimed key.
    let handlers = snapshot.handlers_for(EventTag::Input);
    assert!(
        !handlers.is_empty(),
        "plugin did not subscribe the Input event"
    );

    // 3. Sanity: the extension still registers its slash command + TTS hook, so
    //    a regression that dropped unrelated registration would be caught too.
    assert!(
        snapshot.commands().iter().any(|c| c.name == "voice"),
        "the /voice command went missing"
    );
    assert_eq!(
        snapshot.handlers_for(EventTag::MessageEnd).len(),
        1,
        "the auto-TTS MessageEnd handler went missing"
    );
}

/// A non-claimed key must not be swallowed: `has_shortcut` is the TUI's routing
/// predicate, so verify it only answers true for the claimed key.
#[test]
fn only_the_claimed_key_is_routed() {
    let Some((_, snapshot)) = load_voice() else {
        return;
    };
    assert!(!snapshot.has_shortcut("tab"));
    assert!(!snapshot.has_shortcut("a"));
    assert!(!snapshot.has_shortcut("enter"));
}

/// The spoken-style prompt transform, driven through the real `rpi_voice`
/// cdylib.
///
/// Voice appends a "write for the ear" section to the system prompt while
/// replies are being read aloud, and must leave a typed session untouched. Both
/// halves span the FFI boundary and are invisible to unit tests in either repo:
/// the plugin has to register the `before_agent_start` slot at load, and its
/// handler has to read the envelope + return `systemPrompt` in the shape the
/// host parses. This exercises that end to end against the built artifact.
#[test]
fn voice_registers_a_prompt_transformer_that_respects_the_speech_switch() {
    use rpi_plugin_sdk::{StbString, StbStringRef};

    let Some((_, snapshot)) = load_voice() else {
        eprintln!("rpi_voice cdylib not built — skipping");
        return;
    };

    let handlers = snapshot.before_agent_start();
    assert_eq!(
        handlers.len(),
        1,
        "rpi_voice should register exactly one system-prompt transformer"
    );
    let h = &handlers[0];

    /// Call the plugin's handler the way the host does: envelope in, JSON out.
    fn transform(h: &rpi_extensions::BeforeAgentStartHandler, base: &str) -> serde_json::Value {
        let envelope = serde_json::json!({
            "prompt": "hello",
            "imageCount": 0,
            "systemPrompt": base,
        })
        .to_string();
        let mut out = StbString::empty();
        let rc = (h.handler)(StbStringRef::from_str(&envelope), &mut out, h.user_data);
        assert_eq!(rc, 0, "handler must report success");
        // SAFETY: the ABI callback returned a live output, copied before producer-side cleanup.
        let text = unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(h.plugin_free_string))
        }
        .to_string_lossy();
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("bad payload {text:?}: {e}"))
    }

    // The switch lives in the plugin's process-global state, driven by env at
    // registration; drive it through the same door the plugin reads so the test
    // is independent of test ordering. `/voice on|off` sets the same flag.
    let speech_on = std::env::var("RPI_VOICE_AUTO_TTS")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "on" | "1" | "true" | "yes"
            )
        })
        .unwrap_or(false);
    if !speech_on {
        eprintln!("RPI_VOICE_AUTO_TTS is not on — exercising only the no-change path");
        let payload = transform(h, "BASE");
        assert_eq!(
            payload,
            serde_json::json!({}),
            "with speech off the plugin must report no change"
        );
        return;
    }

    // Speech on: the base prompt survives and the style section is appended.
    let payload = transform(h, "BASE PROMPT");
    let next = payload
        .get("systemPrompt")
        .and_then(|v| v.as_str())
        .expect("speech on must return a replacement prompt");
    assert!(
        next.starts_with("BASE PROMPT"),
        "the host's prompt must be preserved, got: {next}"
    );
    assert!(
        next.to_lowercase().contains("write for the ear"),
        "the spoken style section is missing: {next}"
    );
    // The user's ask, verbatim: acknowledge first, then say what you are about
    // to do. Pin both so a rewrite that drops them is caught here.
    assert!(
        next.contains("Acknowledge first"),
        "missing acknowledgement rule"
    );
    assert!(
        next.contains("Announce before you act"),
        "missing announce rule"
    );

    // Idempotent: a second pass must not duplicate the section.
    let again = transform(h, next);
    let twice = again.get("systemPrompt").and_then(|v| v.as_str()).unwrap();
    assert_eq!(twice, next, "the section must not stack");
}
