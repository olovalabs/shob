use super::*;
use rpi_tui::Component;

mod autocomplete_flicker;
#[cfg(all(feature = "clipboard", not(target_os = "android")))]
mod clipboard_persistence;
mod context_panel;
mod enter_completion;
mod session_regressions;
mod skill_commands;
mod updown_cursor;

#[test]
fn changelog_command_shows_current_release_and_all_is_explicit() {
    let entries = crate::changelog::bundled_entries();
    let latest = crate::changelog::latest_release(&entries, crate::VERSION);
    let heading = |entry: &crate::changelog::ChangelogEntry| {
        format!("{}.{}.{}", entry.major, entry.minor, entry.patch)
    };
    let latest_heading = heading(&latest[0]);
    let historical_heading = heading(entries.last().unwrap());
    let chat = Arc::new(Container::new());
    add_changelog_message(&chat);
    let output = strip_ansi(&chat.render(120).join("\n"));
    assert!(output.lines().any(|line| line.trim() == latest_heading));
    assert!(!output.lines().any(|line| line.trim() == historical_heading));
    assert!(!output.contains("Native parity phase 1"));
    let chat = Arc::new(Container::new());
    add_changelog_message_with_scope(&chat, true);
    let output = strip_ansi(&chat.render(120).join("\n"));
    assert!(output.lines().any(|line| line.trim() == historical_heading));
    assert!(output.lines().any(|line| line.trim() == latest_heading));
}

#[test]
fn startup_changelog_foreign_profile_is_shown_once_and_persisted() {
    const CHILD: &str = "RPI_CHANGELOG_REGRESSION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"lastChangelogVersion":"0.85.1","custom":{"retained":true}}"#,
        )
        .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "interactive_tui::tests::startup_changelog_foreign_profile_is_shown_once_and_persisted", "--nocapture"])
            .env(CHILD, "1").env(crate::config::CONFIG_DIR_ENV, dir.path()).output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let harness = AgentHarness::create(Default::default()).await.unwrap();
            let settings = crate::settings::load_settings().unwrap();
            let chat = Arc::new(Container::new());
            maybe_add_startup_changelog(&harness, &chat, &settings, false, true).await;
            let output = strip_ansi(&chat.render(120).join("\n"));
            assert!(output.contains("What's New"));
            assert!(output.lines().any(|line| line.trim() == crate::VERSION));
            let old = crate::changelog::bundled_entries().pop().unwrap();
            let old_version = format!("{}.{}.{}", old.major, old.minor, old.patch);
            assert!(!output.lines().any(|line| line.trim() == old_version));
            let settings = crate::settings::load_settings().unwrap();
            assert_eq!(
                settings.last_rpi_changelog_version.as_deref(),
                Some(crate::VERSION)
            );
            assert_eq!(settings.last_changelog_version.as_deref(), Some("0.85.1"));
            let chat = Arc::new(Container::new());
            maybe_add_startup_changelog(&harness, &chat, &settings, false, true).await;
            assert_eq!(chat.child_count(), 0);
            let value: serde_json::Value = serde_json::from_slice(
                &std::fs::read(crate::config::settings_path().unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(value["custom"]["retained"], true);
            // A newer process may save a marker after this startup read its settings.
            let mut stale = settings.clone();
            let current_version = semver::Version::parse(crate::VERSION).unwrap();
            let old = crate::changelog::bundled_entries()
                .into_iter()
                .find(|entry| {
                    (entry.major, entry.minor, entry.patch)
                        < (
                            current_version.major,
                            current_version.minor,
                            current_version.patch,
                        )
                })
                .unwrap();
            stale.last_rpi_changelog_version =
                Some(format!("{}.{}.{}", old.major, old.minor, old.patch));
            let mut newer = settings;
            let mut next_version = current_version;
            next_version.patch += 1;
            newer.last_rpi_changelog_version = Some(next_version.to_string());
            crate::settings::save_settings(&newer).unwrap();
            maybe_add_startup_changelog(&harness, &Arc::new(Container::new()), &stale, false, true)
                .await;
            assert_eq!(
                crate::settings::load_settings()
                    .unwrap()
                    .last_rpi_changelog_version,
                Some(next_version.to_string())
            );
        });
}

#[test]
fn collapsed_startup_changelog_links_to_full_notes() {
    let chat = Arc::new(Container::new());
    add_startup_changelog_message(&chat, "## 0.3.17\n\n- full release detail", "0.3.17", true);
    let output = strip_ansi(&chat.render(120).join("\n"));
    assert!(output.contains("Updated to v0.3.17"));
    assert!(output.contains("/changelog"));
    assert!(!output.contains("full release detail"));
    let chat = Arc::new(Container::new());
    add_startup_changelog_message(&chat, "## 0.3.17\n\n- full release detail", "0.3.17", false);
    let output = strip_ansi(&chat.render(120).join("\n"));
    assert!(output.contains("What's New"));
    assert!(output.contains("full release detail"));
}

#[test]
fn queued_user_message_keeps_images_and_image_only_messages() {
    let image = rpi_ai::types::ImageContent {
        kind: rpi_ai::types::ImageContentType,
        data: "test".into(),
        mime_type: "image/png".into(),
    };
    for text in ["", "inspect this"] {
        let message = user_message_with_images(text, vec![image.clone()]);
        let AgentMessage::User(user) = message else {
            panic!("expected user message")
        };
        let rpi_ai::types::UserContent::Blocks(blocks) = user.content else {
            panic!("expected image blocks")
        };
        assert!(
            matches!(blocks.last(), Some(rpi_ai::types::Content::Image(value)) if value == &image)
        );
        assert_eq!(blocks.len(), if text.is_empty() { 1 } else { 2 });
    }
}

#[test]
fn pasted_images_accept_multiple_quoted_paths_without_consuming_normal_text() {
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&[255, 0, 0, 255])
            .unwrap();
    }
    let first = dir.path().join("first image.png");
    let second = dir.path().join("second.png");
    std::fs::write(&first, &bytes).unwrap();
    std::fs::write(&second, &bytes).unwrap();
    let quoted = format!("\"{}\" \"{}\"", first.display(), second.display());
    assert_eq!(images_from_pasted_paths(&quoted).unwrap().len(), 2);
    assert_eq!(
        images_from_pasted_paths(&format!("{}\n{}", first.display(), second.display()))
            .unwrap()
            .len(),
        2
    );
    assert!(images_from_pasted_paths("please inspect these images").is_none());
    let state = test_tui_state();
    let images = images_from_pasted_paths(&quoted).unwrap();
    for image in &images {
        state.queue_image(image.clone());
    }
    let sent = state.take_pending_images();
    assert!(state.take_pending_images().is_empty());
    state.queue_image(images[0].clone());
    state.restore_pending_images(sent);
    assert_eq!(state.take_pending_images().len(), 3);
    state.editor.set_text("existing draft");
    assert!(attach_pasted_images(
        &state,
        &format!("\"{}\"", first.display())
    ));
    assert_eq!(state.take_pending_images().len(), 1);
    assert_eq!(state.editor.get_text(), "existing draft");
    assert!(!attach_pasted_images(&state, "normal clipboard text"));
    assert!(state.take_pending_images().is_empty());
}

#[test]
#[ignore = "set RPI_STATS_DLL to the monitor DLL"]
fn monitor_function_key_works_with_a_draft_without_changing_text() {
    use rpi_extensions::{
        dispatch_empty_event, load_session_mixed, NullDiagnostics, RegistrySnapshot,
    };
    use rpi_plugin_sdk::EventTag;
    struct Shutdown(Arc<RegistrySnapshot>);
    impl Drop for Shutdown {
        fn drop(&mut self) {
            dispatch_empty_event(&self.0, EventTag::SessionShutdown);
        }
    }
    let state = test_tui_state();
    let session = load_session_mixed(
        &[],
        &[std::path::PathBuf::from(
            std::env::var("RPI_STATS_DLL").unwrap(),
        )],
        Arc::new(NullDiagnostics),
        None,
    );
    let snapshot = session.snapshot_arc().expect("monitor loaded");
    *state.extension_session.lock().unwrap() = session;
    let _shutdown = Shutdown(snapshot);
    state.editor.set_text("unsent draft");
    assert!(dispatch_key_event(
        &state,
        &KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE),
        &state.editor
    ));
    assert_eq!(state.editor.get_text(), "unsent draft");
    assert!(!dispatch_key_event(
        &state,
        &KeyEvent::new(KeyCode::F(8), KeyModifiers::CONTROL),
        &state.editor
    ));
    assert!(!dispatch_key_event(
        &state,
        &KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        &state.editor
    ));
    assert_eq!(state.editor.get_text(), "unsent draft");
}

#[test]
fn modal_inputs_keep_registered_plugin_shortcuts_until_closed() {
    use rpi_extensions::{ExtensionRegistry, ExtensionSession, PluginKeepalive};
    use rpi_plugin_sdk::{EventTag, StablePluginEvent, EVENT_HANDLER_CLAIMED};
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CONTEXTS: rpi_plugin_sdk::handles::ToolHandles<Arc<AtomicUsize>> =
        rpi_plugin_sdk::handles::ToolHandles::new();
    struct ContextOwner(usize);
    impl Drop for ContextOwner {
        fn drop(&mut self) {
            drop(CONTEXTS.remove(self.0 as rpi_plugin_sdk::StepHandle));
        }
    }
    extern "C" fn claim(_: StablePluginEvent, user_data: *mut std::ffi::c_void) -> i32 {
        let context = CONTEXTS.get(user_data).expect("live test context");
        let calls = context.lock().clone();
        calls.fetch_add(1, Ordering::SeqCst);
        EVENT_HANDLER_CLAIMED
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let context = CONTEXTS.insert(calls.clone());
    let _owner = ContextOwner(context as usize);
    let state = test_tui_state();
    let mut registry = ExtensionRegistry::new();
    for name in ["space", "f8", "escape", "enter"] {
        registry.register_shortcut("test".into(), name.into(), "test".into());
    }
    registry.register_event_handler("test".into(), EventTag::Input, claim, context);
    *state.extension_session.lock().unwrap() = ExtensionSession::from_parts(
        Arc::new(registry.snapshot()),
        PluginKeepalive::empty(),
        vec![],
        None,
    );
    let keys = [KeyCode::Char(' '), KeyCode::F(8), KeyCode::Enter];
    for code in keys {
        assert!(dispatch_key_event(
            &state,
            &KeyEvent::new(code, KeyModifiers::NONE),
            &state.editor
        ));
        assert!(dispatch_key_event(
            &state,
            &KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Release),
            &state.editor
        ));
    }
    let before = calls.load(Ordering::SeqCst);
    for modal in 0..4 {
        match modal {
            0 => *state.active_extension_editor.lock().unwrap() = Some(Arc::new(Editor::simple())),
            1 => *state.active_extension_input.lock().unwrap() = Some(Arc::new(Input::new())),
            2 => {
                *state.active_selector.lock().unwrap() = Some((
                    SelectorView::List(Arc::new(SelectList::new(vec![], 5))),
                    SelectorKind::Extension,
                ))
            }
            _ => state.search.activate(),
        }
        for code in keys.into_iter().chain([KeyCode::Esc]) {
            for kind in [
                KeyEventKind::Press,
                KeyEventKind::Repeat,
                KeyEventKind::Release,
            ] {
                let key = KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind);
                assert!(!dispatch_key_event(&state, &key, &state.editor));
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), before);
        state.active_extension_editor.lock().unwrap().take();
        state.active_extension_input.lock().unwrap().take();
        state.active_selector.lock().unwrap().take();
        state.search.deactivate();
    }
    assert!(dispatch_key_event(
        &state,
        &KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE),
        &state.editor
    ));
    assert_eq!(calls.load(Ordering::SeqCst), before + 1);
    let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
    assert!(dispatch_key_event(&state, &space, &state.editor));
    state.editor.set_text("injected transcription");
    *state.active_extension_input.lock().unwrap() = Some(Arc::new(Input::new()));
    let held = calls.load(Ordering::SeqCst);
    assert!(dispatch_key_event(
        &state,
        &KeyEvent::new_with_kind(space.code, space.modifiers, KeyEventKind::Release),
        &state.editor
    ));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        held + 1,
        "finish the already-owned hold"
    );
    assert!(!dispatch_key_event(&state, &space, &state.editor));
}

/// A draft injected from a voice transcription is submitted only when it is
/// still exactly what we put in the editor and no run has started; any
/// user edit (or a cleared editor) hands it back for editing instead.
#[test]
fn auto_send_cancels_when_the_user_edits_the_draft() {
    let now = std::time::Instant::now();
    let pending = AutoSendPending {
        deadline: now + std::time::Duration::from_secs(2),
        text: "hello world".to_string(),
    };
    // Untouched → still counting down.
    assert!(matches!(
        auto_send_step(Some(&pending), "hello world", true, now),
        AutoSendStep::Wait(_)
    ));
    // One added character is the user's correction: never send a draft
    // they are actively editing.
    assert_eq!(
        auto_send_step(Some(&pending), "hello worlds", true, now),
        AutoSendStep::Cancel
    );
    // Cleared out from under us (a slash command, Esc).
    assert_eq!(
        auto_send_step(Some(&pending), "", true, now),
        AutoSendStep::Cancel
    );
    // Whitespace is not content.
    assert_eq!(
        auto_send_step(Some(&pending), "   ", true, now),
        AutoSendStep::Cancel
    );
}

/// The countdown fires exactly at the deadline, and never injects a prompt
/// into a run that started while it was ticking.
#[test]
fn auto_send_submits_at_the_deadline_only_while_idle() {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
    let pending = AutoSendPending {
        deadline,
        text: "go".to_string(),
    };
    assert!(matches!(
        auto_send_step(
            Some(&pending),
            "go",
            true,
            deadline - std::time::Duration::from_millis(1)
        ),
        AutoSendStep::Wait(_)
    ));
    assert_eq!(
        auto_send_step(Some(&pending), "go", true, deadline),
        AutoSendStep::Submit
    );
    // A turn started underneath the countdown — keep the draft, do not
    // queue a prompt mid-turn.
    assert_eq!(
        auto_send_step(Some(&pending), "go", false, deadline),
        AutoSendStep::Cancel
    );
    // Nothing pending.
    assert_eq!(
        auto_send_step(None, "go", true, deadline),
        AutoSendStep::Idle
    );
}

/// The footer text the countdown writes is prefixed so `clear_auto_send`
/// can tell its own message from an unrelated run status.
#[test]
fn auto_send_footer_label_is_recognisable() {
    let label = format!("{AUTO_SEND_PREFIX}2.0s · any key to edit");
    assert!(label.starts_with(AUTO_SEND_PREFIX));
    assert!(!"Working…".starts_with(AUTO_SEND_PREFIX));
}

#[test]
fn markdown_flag_only_comes_from_an_explicit_true() {
    // Tools opt in by setting `details.markdown = true` (rpi-todo does).
    assert!(tool_result_requests_markdown(Some(
        &serde_json::json!({"kind": "todo", "markdown": true})
    )));
    // Absence, a false value, or a non-bool value keeps plain rendering,
    // so every other tool is unaffected.
    assert!(!tool_result_requests_markdown(None));
    assert!(!tool_result_requests_markdown(Some(
        &serde_json::json!({"kind": "todo"})
    )));
    assert!(!tool_result_requests_markdown(Some(
        &serde_json::json!({"markdown": false})
    )));
    assert!(!tool_result_requests_markdown(Some(
        &serde_json::json!({"markdown": "true"})
    )));
}

/// Minimal `TuiState` for unit tests that only touch state flags. Mirrors
/// the explicit literals other tests build, but keeps one copy in sync.
pub(super) fn test_tui_state() -> Arc<TuiState> {
    Arc::new(TuiState {
        history_page: history::HistoryUi::default(),
        session_dir: std::path::PathBuf::from("unused-session-root"),
        current_assistant: Arc::new(std::sync::Mutex::new(None)),
        tool_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        bash_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        hide_thinking: Arc::new(std::sync::Mutex::new(false)),
        tool_outputs_expanded: Arc::new(std::sync::Mutex::new(false)),
        show_terminal_progress: true,
        status: std::sync::Mutex::new(RunStatus::Idle),
        ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
        ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
        editor_text: rpi_extensions::EditorTextMailbox::new(),
        auto_send: std::sync::Mutex::new(None),
        last_editor_text: std::sync::Mutex::new(String::new()),
        programmatic_editor_write: std::sync::atomic::AtomicBool::new(false),
        js_preparation_cancel: std::sync::Mutex::new(None),
        user_bash_cancel: std::sync::Mutex::new(None),
        pending_bash_messages: std::sync::Mutex::new(Vec::new()),
        footer: Arc::new(FooterComponent::new()),
        status_container: Arc::new(Container::new()),
        chat_container: Arc::new(Container::new()),
        loader: Arc::new(Loader::new()),
        editor: Arc::new(Editor::simple()),
        last_assistant_text: std::sync::Mutex::new(String::new()),
        active_selector: std::sync::Mutex::new(None),
        provider_form: std::sync::Mutex::new(None),
        active_provider_input: std::sync::Mutex::new(None),
        provider_auth_flow_id: std::sync::atomic::AtomicU64::new(0),
        provider_auth_generation: std::sync::atomic::AtomicU64::new(0),
        provider_auth_cancel: std::sync::Mutex::new(None),
        active_provider_select: std::sync::Mutex::new(None),
        active_extension_editor: std::sync::Mutex::new(None),
        active_extension_input: std::sync::Mutex::new(None),
        active_extension_cancel: std::sync::Mutex::new(None),
        autocomplete: AutocompleteManager::new(),
        autocomplete_container: Arc::new(Container::new()),
        autocomplete_max_visible: 5,
        autocomplete_selection: std::sync::Mutex::new(0),
        autocomplete_selection_stamped_for: std::sync::Mutex::new(None),
        pending_images: std::sync::Mutex::new(Vec::new()),
        theme_manager: Arc::new(ThemeManager::new()),
        active_theme_spec: std::sync::Mutex::new(None),
        theme_watcher: std::sync::Mutex::new(None),
        theme_registry: std::sync::RwLock::new(crate::themes::ThemeRegistry::default()),
        tui: None,
        current_model_id: std::sync::Mutex::new(String::new()),
        show_images: std::sync::Mutex::new(true),
        cache_miss_notices: std::sync::Mutex::new(false),
        history: std::sync::Mutex::new(Vec::new()),
        history_index: std::sync::Mutex::new(-1),
        history_draft: std::sync::Mutex::new(None),
        cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
        events_lagged: std::sync::atomic::AtomicBool::new(false),
        scoped_edit: std::sync::Mutex::new(None),
        markdown_transformer: Arc::new(std::sync::Mutex::new(None)),
        extension_claimed_keys: Mutex::new(Default::default()),
        extension_session: Arc::new(std::sync::Mutex::new(
            rpi_extensions::ExtensionSession::none(),
        )),
        search: Arc::new(AltScreenSearch::new()),
        search_bar: Arc::new(SearchBar::new()),
        pending_container: Arc::new(Container::new()),
        dequeue_hint: String::new(),
        pending_snapshot: std::sync::Mutex::new(
            rpi_harness::agent_harness::QueuedMessages::default(),
        ),
    })
}

/// A `SetEditorText` injection — and the host's own auto-send clear — must be
/// attributed to `extension`, not to the user.
///
/// A hands-free voice extension stands down whenever `source == "user"`
/// (the human took the keyboard), so mislabelling the plugin's own injected
/// transcription would cancel the whole conversation on its first turn.
#[test]
fn editor_change_source_separates_tui_writes_from_typing() {
    use std::sync::atomic::Ordering;
    let state = test_tui_state();

    // Nothing changed yet → nothing to report.
    assert!(state.take_editor_change().is_none());

    // The render tick applies an extension's draft…
    state.editor_text.push(rpi_extensions::EditorTextEdit {
        text: "你好".to_string(),
        mode: rpi_extensions::EditorTextMode::Replace,
        auto_send_ms: Some(2000),
    });
    assert!(state.drain_editor_text(), "the queued draft must land");
    assert!(
        state.programmatic_editor_write.load(Ordering::SeqCst),
        "a mailbox-applied edit must be marked programmatic"
    );

    // …so its change is ours, not the user's.
    let payload = state
        .take_editor_change()
        .expect("draft applied ⇒ a change");
    assert_eq!(payload["source"], serde_json::json!("extension"));
    assert_eq!(payload["chars"], serde_json::json!(2));
    assert_eq!(payload["empty"], serde_json::json!(false));

    // The mark was consumed: the same text twice is not re-reported, and a
    // genuine human edit is attributed to the user.
    assert!(state.take_editor_change().is_none());
    state.editor.set_text("你好啊");
    let payload = state.take_editor_change().expect("typing ⇒ a change");
    assert_eq!(payload["source"], serde_json::json!("user"));
    assert_eq!(payload["chars"], serde_json::json!(3));
}

/// The attribution mark must not survive unrelated changes, or one injected
/// draft would make the *next* keystroke look programmatic too.
#[test]
fn programmatic_mark_is_consumed_even_with_no_subscriber() {
    use std::sync::atomic::Ordering;
    let state = test_tui_state();
    assert!(
        state
            .extension_session
            .lock()
            .unwrap()
            .snapshot_arc()
            .is_none(),
        "this test relies on nobody subscribing"
    );

    state.editor_text.push(rpi_extensions::EditorTextEdit {
        text: "draft".to_string(),
        mode: rpi_extensions::EditorTextMode::Replace,
        auto_send_ms: None,
    });
    state.drain_editor_text();
    // `sync_editor_change` bails out early (no subscriber)…
    state.sync_editor_change();
    // …but the mark is gone, so the next real edit is the user's.
    assert!(!state.programmatic_editor_write.load(Ordering::SeqCst));
    state.editor.set_text("draft +");
    let payload = state.take_editor_change().expect("typing ⇒ a change");
    assert_eq!(payload["source"], serde_json::json!("user"));
}

/// Regression: the editor clear that follows *every* submit is host
/// housekeeping, not the user typing.
///
/// Reporting it as `source:"user"` switched a hands-free voice session off
/// the instant it was switched on: `/voice auto` turns the mode on, then the
/// render tick notices the cleared editor ~80ms later, stands the mode down
/// and aborts the listen it just started — so the microphone never stayed
/// open and the session looked like it "didn't hear anything".
#[test]
fn submit_clear_is_attributed_to_the_host_not_the_user() {
    let state = test_tui_state();

    // Typing the command is a genuine user edit.
    state.editor.set_text("/voice auto");
    assert_eq!(
        state.take_editor_change().unwrap()["source"],
        serde_json::json!("user")
    );

    // The submit path's clear is not.
    state.clear_editor_programmatically();
    let payload = state.take_editor_change().expect("clearing is a change");
    assert_eq!(payload["empty"], serde_json::json!(true));
    assert_eq!(
        payload["source"],
        serde_json::json!("extension"),
        "a submit-clear must not be reported as the user typing"
    );
}

/// A user-initiated clear (Esc, select-all + delete) still goes through the
/// editor itself and must stay `user` — only host writes are reattributed.
#[test]
fn user_initiated_clear_stays_a_user_change() {
    let state = test_tui_state();
    state.editor.set_text("draft");
    assert_eq!(
        state.take_editor_change().unwrap()["source"],
        serde_json::json!("user")
    );
    state.editor.clear();
    let payload = state.take_editor_change().expect("clearing is a change");
    assert_eq!(payload["source"], serde_json::json!("user"));
}

#[test]
fn live_panel_repaints_while_a_tool_or_bash_is_running() {
    // No panels → only the dock loader animates (reuse the cached
    // transcript instead of rebuilding a long history).
    let empty: HashMap<String, Arc<ToolExecutionComponent>> = HashMap::new();
    assert!(!transcript_has_live_panel(false, &empty));

    // A running tool call is a live panel: its elapsed readout is computed
    // at render time, so it must force a transcript rebuild every tick.
    let mut tools = HashMap::new();
    let tool = Arc::new(ToolExecutionComponent::new("search", "{}"));
    tool.set_running();
    tools.insert("tc1".to_string(), tool);
    assert!(transcript_has_live_panel(false, &tools));

    // A finished tool no longer needs the repaint, but a running bash
    // command still does.
    tools.get("tc1").unwrap().set_result("done", false);
    assert!(!transcript_has_live_panel(false, &tools));
    assert!(transcript_has_live_panel(true, &tools));
}

#[test]
fn dequeue_merge_preserves_a_typed_draft() {
    assert_eq!(merge_queued_into_draft("queued", ""), "queued");
    assert_eq!(merge_queued_into_draft("queued", "   "), "queued");
    assert_eq!(
        merge_queued_into_draft("queued", "typed"),
        "queued\n\ntyped"
    );
}

#[test]
fn dequeue_places_the_caret_at_the_end_of_the_last_line_in_bytes() {
    // Restoring queued messages followed by the old `(0, char_count)` call
    // parked the caret at the end of the FIRST line, and treated a char
    // count as a byte offset (so any CJK draft landed mid-character).
    let editor = Arc::new(Editor::simple());
    // Multi-line + multibyte: the caret must be on the LAST row, after the
    // final byte of that row.
    let text = "排队\n第二行";
    set_editor_text_caret_at_end(&editor, text);
    assert_eq!(editor.get_text(), text);
    assert_eq!(
        editor.cursor_position(),
        (1, "第二行".len()),
        "caret must sit at the end of the last line, in bytes"
    );
}

#[tokio::test]
async fn dequeue_restores_both_queues_images_and_folded_draft_without_aborting() {
    let harness = AgentHarness::create(Default::default()).await.unwrap();
    let lane: Arc<dyn AgentLane> = harness.lane("main");
    let state = test_tui_state();
    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        true,
        None,
    ));
    let image = rpi_ai::types::ImageContent {
        kind: rpi_ai::types::ImageContentType,
        data: "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7".into(),
        mime_type: "image/gif".into(),
    };
    lane.steer(user_message_with_images("排队修改", vec![image.clone()]))
        .await
        .unwrap();
    lane.follow_up(user_message_with_images("后续消息", vec![]))
        .await
        .unwrap();
    refresh_pending_messages(&state, &lane).await;
    let draft = "草稿\n".repeat(30);
    state.editor.handle_paste(&draft);
    state.set_status(RunStatus::Working);
    handle_dequeue(lane.clone(), state.editor.clone(), state.clone(), tui).await;
    let expected = format!("排队修改\n\n后续消息\n\n{draft}");
    assert_eq!(state.editor.get_expanded_text(), expected);
    assert_eq!(
        state.editor.cursor_position(),
        (expected.split('\n').count() - 1, 0)
    );
    assert_eq!(*state.pending_images.lock().unwrap(), vec![image]);
    assert!(lane.queued_messages().await.unwrap().is_empty());
    assert!(state.pending_snapshot.lock().unwrap().is_empty());
    assert!(*state.status.lock().unwrap() == RunStatus::Working);
    assert!(strip_ansi(&state.chat_container.render(120).join("\n"))
        .contains("Restored 2 queued messages to editor"));
    harness.close().await.unwrap();
}

#[test]
fn hot_cycle_crosses_providers_and_anchors_duplicate_ids_to_the_live_lane() {
    with_isolated_config_dir(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let harness = AgentHarness::create(Default::default()).await.unwrap();
                let lane: Arc<dyn AgentLane> = harness.lane("main");
                let state = test_tui_state();
                let first = rpi_ai::Model::new(
                    "shared",
                    "First",
                    rpi_ai::Api::OpenaiCompletions,
                    "first",
                    "http://first.invalid",
                );
                let second = rpi_ai::Model::new(
                    "shared",
                    "Second",
                    rpi_ai::Api::OpenaiCompletions,
                    "second",
                    "http://second.invalid",
                );
                let mut responses = second.clone();
                responses.api = rpi_ai::Api::OpenaiResponses;
                let catalog = [first.clone(), second.clone(), responses.clone()];
                lane.set_model(first.clone()).await.unwrap();
                let next = cycle_model(&lane, &catalog, false).await.unwrap().unwrap();
                assert_eq!(next, second);
                let note = apply_model_selection(&lane, &state, next, false)
                    .await
                    .unwrap();
                assert!(note.contains("second/shared"));
                assert!(
                    note.contains("Not saved as the default"),
                    "the note should say the switch was not persisted: {note}"
                );
                assert_eq!(lane.get_model().await.unwrap(), second);
                assert!(
                    strip_ansi(&state.footer.render(120).join("\n")).contains("(second) shared")
                );
                let saved = crate::settings::load_settings().unwrap();
                assert_eq!(
                    saved.default_provider.as_deref(),
                    None,
                    "a session-scoped switch must not save a default provider"
                );
                assert_eq!(
                    saved.default_model.as_deref(),
                    None,
                    "a session-scoped switch must not save a default model"
                );
                assert_eq!(
                    cycle_model(&lane, &catalog, true).await.unwrap().unwrap(),
                    first
                );
                assert_eq!(
                    cycle_model(&lane, &catalog, false).await.unwrap().unwrap(),
                    responses
                );
                lane.set_model(responses).await.unwrap();
                assert_eq!(
                    cycle_model(&lane, &catalog, false).await.unwrap().unwrap(),
                    first
                );
                let mut settings = saved.clone();
                settings.scoped_models = Some(vec!["first/shared".into()]);
                crate::settings::save_settings(&settings).unwrap();
                assert_eq!(
                    cycle_model(&lane, &catalog, false).await.unwrap().unwrap(),
                    first
                );
                assert!(cycle_model(&lane, &[], false).await.unwrap().is_none());
                let old_id = state.current_model_id();
                let old_footer = state.footer.render(120);
                harness.close().await.unwrap();
                assert!(apply_model_selection(&lane, &state, first, true)
                    .await
                    .is_err());
                assert_eq!(state.current_model_id(), old_id);
                assert_eq!(state.footer.render(120), old_footer);
                assert_eq!(
                    crate::settings::load_settings().unwrap().default_provider,
                    saved.default_provider
                );
            });
    });
}

/// The only path allowed to touch `settings.json`: `/settings` → Default model
/// passes `save_default = true` and must still persist the pair.
#[test]
fn explicit_default_model_selection_still_persists() {
    with_isolated_config_dir(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let harness = AgentHarness::create(Default::default()).await.unwrap();
                let lane: Arc<dyn AgentLane> = harness.lane("main");
                let state = test_tui_state();
                let model = rpi_ai::Model::new(
                    "saved-model",
                    "Saved",
                    rpi_ai::Api::OpenaiCompletions,
                    "saved",
                    "http://saved.invalid",
                );
                lane.set_model(model.clone()).await.unwrap();
                let note = apply_model_selection(&lane, &state, model.clone(), true)
                    .await
                    .unwrap();
                assert!(note.contains("saved/saved-model"), "{note}");
                assert!(note.contains("Default saved."), "{note}");
                assert_eq!(lane.get_model().await.unwrap(), model);
                let saved = crate::settings::load_settings().unwrap();
                assert_eq!(saved.default_provider.as_deref(), Some("saved"));
                assert_eq!(saved.default_model.as_deref(), Some("saved-model"));
                harness.close().await.unwrap();
            });
    });
}

#[test]
fn settings_menu_covers_the_wired_settings_surface() {
    let state = test_tui_state();
    let settings = crate::settings::Settings::default();
    let items = settings_menu_items(&settings, &state, "claude-sonnet-5");
    let keys: Vec<&str> = items.iter().map(|item| item.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "theme",
            "model",
            "thinking",
            "scoped-models",
            "hide-thinking",
            "show-images",
            "cache-miss-notices",
            "quiet-startup",
            "collapse-changelog",
            "terminal-progress",
            "fullscreen-copy-on-select",
            "double-escape-action",
            "editor-padding",
            "autocomplete-max-items",
            "http-idle-timeout",
        ]
    );
    // Submenu rows must not cycle their displayed value.
    for item in items.iter().filter(|item| item.has_submenu) {
        assert!(
            item.values.is_empty(),
            "{} should be submenu-only",
            item.key
        );
    }
    // Value rows advertise a cycle that contains the current value.
    for item in items.iter().filter(|item| !item.has_submenu) {
        assert!(!item.values.is_empty(), "{} has no values", item.key);
        assert!(
            item.values.iter().any(|value| value == &item.value),
            "{} current value {} not in {:?}",
            item.key,
            item.value,
            item.values
        );
    }
    // The default model row falls back to the running lane's model.
    let value = |key: &str| {
        items
            .iter()
            .find(|item| item.key == key)
            .unwrap()
            .value
            .as_str()
    };
    assert_eq!(value("model"), "claude-sonnet-5");
    // Native defaults: cache-miss notices off, terminal progress on.
    assert_eq!(value("cache-miss-notices"), "false");
    assert_eq!(value("terminal-progress"), "true");
    assert_eq!(value("collapse-changelog"), "false");
}

#[test]
fn enabling_images_on_an_unsupported_terminal_says_so() {
    // Driven through the public setting entry point, and the expectation is
    // derived from the same capability the note is built from, so the probe is
    // deterministic on any developer terminal (Kitty/Ghostty/WezTerm on CI).
    // It is a behaviour assertion, not a compile assertion: on the previous
    // code the note is always "Inline images enabled." and the `None` cell
    // below fails.
    let state = test_tui_state();
    let mut settings = crate::settings::Settings::default();
    let supported = rpi_tui::get_capabilities().protocol != rpi_tui::ImageProtocol::None;

    let note = apply_setting_change(&state, "show-images", "true", &mut settings)
        .expect("user-visible note");
    assert_eq!(settings.show_images, Some(true));
    assert!(*state.show_images.lock().unwrap());

    if supported {
        assert_eq!(note, "Inline images enabled.");
    } else {
        assert!(
            note.contains("cannot display them"),
            "a terminal without an image protocol must not be told images are on: {note}"
        );
        assert!(
            note.contains("placeholder"),
            "the note must say what the user will see instead: {note}"
        );
    }

    // Disabling never carries the capability warning: it is always accurate.
    let note = apply_setting_change(&state, "show-images", "false", &mut settings)
        .expect("user-visible note");
    assert_eq!(note, "Inline images disabled.");
    assert!(!*state.show_images.lock().unwrap());
}

#[test]
fn apply_setting_change_updates_settings_and_live_state() {
    let state = test_tui_state();
    let mut settings = crate::settings::Settings::default();

    // Live: hide-thinking flips both the runtime flag and the persisted one.
    let note = apply_setting_change(&state, "hide-thinking", "true", &mut settings);
    assert!(state.hide_thinking());
    assert_eq!(settings.hide_thinking_block, Some(true));
    assert!(note.expect("user-visible note").contains("hidden"));

    // Live: show-images writes the runtime mutex.
    apply_setting_change(&state, "show-images", "false", &mut settings);
    assert!(!*state.show_images.lock().unwrap());
    assert_eq!(settings.show_images, Some(false));

    // Live: cache-miss notices.
    apply_setting_change(&state, "cache-miss-notices", "true", &mut settings);
    assert!(*state.cache_miss_notices.lock().unwrap());
    assert_eq!(settings.show_cache_miss_notices, Some(true));

    // Persist-only rows change settings without a transcript note.
    assert_eq!(
        apply_setting_change(&state, "editor-padding", "3", &mut settings),
        None
    );
    assert_eq!(settings.editor_padding_x, Some(3));
    apply_setting_change(&state, "double-escape-action", "none", &mut settings);
    assert_eq!(settings.double_escape_action.as_deref(), Some("none"));
    apply_setting_change(&state, "autocomplete-max-items", "20", &mut settings);
    assert_eq!(settings.autocomplete_max_visible, Some(20));

    // HTTP idle timeout maps labels onto the numeric/`disabled` forms.
    apply_setting_change(&state, "http-idle-timeout", "disabled", &mut settings);
    assert_eq!(settings.http_idle_timeout_ms(), Some(0));
    apply_setting_change(&state, "http-idle-timeout", "1m", &mut settings);
    assert_eq!(settings.http_idle_timeout_ms(), Some(60_000));

    // Unknown keys are ignored rather than panicking.
    assert_eq!(
        apply_setting_change(&state, "nope", "x", &mut settings),
        None
    );
}

#[test]
fn add_user_message_renders_a_visible_user_bubble() {
    // The submit handler renders directly-sent prompts through this helper
    // (the harness emits no user `message_start` for them), so a silent
    // failure here would make every prompt vanish from the transcript.
    let chat = Arc::new(Container::new());
    add_user_message(&chat, "hello from the user");
    let rendered = crate::interactive_tui::collect_transcript_lines(&chat);
    let plain = strip_ansi(&rendered.join("\n"));
    assert!(
        plain.contains("hello from the user"),
        "user bubble must show the prompt text; got: {plain:?}"
    );
}

#[test]
fn verbose_overrides_quiet_startup_listing() {
    assert!(should_show_startup_listing(false, false));
    assert!(should_show_startup_listing(true, false));
    assert!(should_show_startup_listing(true, true));
    assert!(!should_show_startup_listing(false, true));
}

#[test]
fn resume_command_omits_the_default_session_dir() {
    let _env = crate::config::test_support::SessionDirEnv::new(None);
    let cwd = std::path::Path::new("/proj/demo");
    assert_eq!(format_resume_command("", cwd, None), None);
    assert_eq!(
        format_resume_command("abc-123", cwd, None).as_deref(),
        Some("rpi --session abc-123")
    );
    // The default dir carries no flag (mirrors upstream).
    let default = crate::session::default_session_dir(cwd);
    assert_eq!(
        format_resume_command("abc-123", cwd, Some(default.as_path())).as_deref(),
        Some("rpi --session abc-123")
    );
    // A custom dir is echoed, quoting when needed.
    let custom = std::path::Path::new("/tmp/my sessions");
    assert_eq!(
        format_resume_command("abc-123", cwd, Some(custom)).as_deref(),
        Some("rpi --session-dir \"/tmp/my sessions\" --session abc-123")
    );
}

#[test]
fn quote_shell_arg_quotes_only_when_needed() {
    assert_eq!(quote_shell_arg("plain"), "plain");
    assert_eq!(quote_shell_arg("with space"), "\"with space\"");
    assert_eq!(quote_shell_arg("a&b"), "\"a&b\"");
}

#[test]
fn trust_command_uses_the_session_cwd_after_process_chdir() {
    const CHILD_ENV: &str = "RPI_TEST_TRUST_COMMAND_CHILD";
    const SESSION_CWD_ENV: &str = "RPI_TEST_TRUST_COMMAND_SESSION_CWD";
    const TEST_NAME: &str =
        "interactive_tui::tests::trust_command_uses_the_session_cwd_after_process_chdir";

    if std::env::var_os(CHILD_ENV).is_some() {
        let session_cwd = std::path::PathBuf::from(
            std::env::var_os(SESSION_CWD_ENV).expect("child session cwd should be configured"),
        );
        let changed_cwd = std::env::current_dir().unwrap();

        set_project_trust_for_command(&session_cwd, Some(true)).unwrap();

        assert_eq!(
            crate::config::project_trust_decision(&session_cwd).unwrap(),
            Some(true)
        );
        assert_eq!(
            crate::config::project_trust_decision(&changed_cwd).unwrap(),
            None
        );
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let session_cwd = tmp.path().join("session-project");
    let changed_cwd = tmp.path().join("extension-cwd");
    let agent_dir = tmp.path().join("agent");
    std::fs::create_dir_all(&session_cwd).unwrap();
    std::fs::create_dir_all(&changed_cwd).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();

    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(TEST_NAME)
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .env(SESSION_CWD_ENV, &session_cwd)
        .env(crate::config::CONFIG_DIR_ENV, &agent_dir)
        .current_dir(&changed_cwd)
        .status()
        .unwrap();

    assert!(status.success(), "child test process failed: {status}");
}

#[test]
fn tui_startup_settings_prefer_rpi_project_fields_over_pi() {
    let global = crate::settings::Settings {
        editor_padding_x: Some(3),
        autocomplete_max_visible: Some(6),
        hide_thinking_block: Some(false),
        quiet_startup: Some(true),
        show_terminal_progress: Some(true),
        ..Default::default()
    };
    let rpi_project = crate::settings::Settings {
        editor_padding_x: Some(0),
        hide_thinking_block: Some(true),
        quiet_startup: Some(false),
        show_terminal_progress: Some(false),
        ..Default::default()
    };
    let pi_project = crate::settings::Settings {
        editor_padding_x: Some(9),
        autocomplete_max_visible: Some(12),
        quiet_startup: Some(true),
        ..Default::default()
    };

    assert_eq!(
        resolve_tui_startup_settings(&global, &[rpi_project, pi_project], true),
        TuiStartupSettings {
            editor_padding_x: 0,
            autocomplete_max_visible: 12,
            hide_thinking: true,
            quiet_startup: false,
            show_terminal_progress: false,
            show_images: true,
            cache_miss_notices: false,
        }
    );
}

#[test]
fn tui_startup_settings_fall_back_from_rpi_to_pi_per_field() {
    let global = crate::settings::Settings {
        quiet_startup: Some(false),
        ..Default::default()
    };
    let rpi_project = crate::settings::Settings::default();
    let pi_project = crate::settings::Settings {
        quiet_startup: Some(true),
        ..Default::default()
    };

    let resolved = resolve_tui_startup_settings(&global, &[rpi_project, pi_project], true);

    assert!(resolved.quiet_startup);
}

#[test]
fn tui_startup_settings_use_global_values_without_project_fields() {
    let global = crate::settings::Settings {
        editor_padding_x: Some(7),
        autocomplete_max_visible: Some(8),
        hide_thinking_block: Some(true),
        quiet_startup: Some(true),
        show_terminal_progress: Some(false),
        show_images: Some(false),
        show_cache_miss_notices: Some(true),
        ..Default::default()
    };

    assert_eq!(
        resolve_tui_startup_settings(&global, &[crate::settings::Settings::default()], true,),
        TuiStartupSettings {
            editor_padding_x: 7,
            autocomplete_max_visible: 8,
            hide_thinking: true,
            quiet_startup: true,
            show_terminal_progress: false,
            show_images: false,
            cache_miss_notices: true,
        }
    );
}

#[test]
fn tui_startup_settings_read_native_nested_terminal_block() {
    // upstream nests these under `terminal`; rpi's flat keys stay
    // compatible and the nested block is the fallback.
    let global = crate::settings::Settings {
        terminal: Some(crate::settings::TerminalSettings {
            show_images: Some(false),
            show_terminal_progress: Some(false),
            clear_on_shrink: None,
        }),
        ..Default::default()
    };
    let resolved =
        resolve_tui_startup_settings(&global, &[crate::settings::Settings::default()], true);
    assert!(!resolved.show_images);
    assert!(!resolved.show_terminal_progress);
}

#[test]
fn tui_startup_settings_flat_keys_win_over_nested_terminal_block() {
    let global = crate::settings::Settings {
        show_images: Some(true),
        terminal: Some(crate::settings::TerminalSettings {
            show_images: Some(false),
            show_terminal_progress: None,
            clear_on_shrink: None,
        }),
        ..Default::default()
    };
    let resolved =
        resolve_tui_startup_settings(&global, &[crate::settings::Settings::default()], true);
    assert!(resolved.show_images);
}

#[test]
fn tui_startup_settings_ignore_untrusted_project_values() {
    let global = crate::settings::Settings {
        quiet_startup: Some(false),
        show_terminal_progress: Some(true),
        ..Default::default()
    };
    let project = crate::settings::Settings {
        quiet_startup: Some(true),
        show_terminal_progress: Some(false),
        ..Default::default()
    };

    let resolved = resolve_tui_startup_settings(&global, &[project], false);

    assert!(!resolved.quiet_startup);
    assert!(resolved.show_terminal_progress);
}

#[test]
fn transcript_page_uses_viewport_with_overlap() {
    assert_eq!(transcript_page_size(24), 20);
    assert_eq!(transcript_page_size(4), 1);
    assert_eq!(transcript_page_size(0), 1);
}

#[test]
fn parse_user_bash_distinguishes_single_and_double_bang() {
    // `!cmd` runs and stays in context.
    assert_eq!(parse_user_bash("!ls -la"), Some(("ls -la", false)));
    // `!!cmd` runs but is excluded from context.
    assert_eq!(parse_user_bash("!!ls -la"), Some(("ls -la", true)));
    // Surrounding whitespace on the command is trimmed, the `!` prefix is not.
    assert_eq!(parse_user_bash("!  echo hi  "), Some(("echo hi", false)));
    assert_eq!(parse_user_bash("!!  echo hi"), Some(("echo hi", true)));
}

#[test]
fn parse_user_bash_ignores_bare_bang_and_plain_text() {
    // A bare `!` / `!!` has no command: fall through to the prompt path
    // instead of executing an empty shell line.
    assert_eq!(parse_user_bash("!"), None);
    assert_eq!(parse_user_bash("!!"), None);
    assert_eq!(parse_user_bash("!   "), None);
    assert_eq!(parse_user_bash("hello"), None);
    assert_eq!(parse_user_bash(""), None);
    // `/` keeps routing to the slash-command registry.
    assert_eq!(parse_user_bash("/model"), None);
}

#[test]
fn user_bash_run_slot_guards_and_cancels() {
    let state = test_tui_state();
    assert!(!state.user_bash_running());
    let cancel = state.begin_user_bash();
    assert!(state.user_bash_running());
    // Esc cancels without freeing the slot; the spawned task clears it when
    // the capture resolves (upstream's `isBashRunning`).
    assert!(state.cancel_user_bash());
    assert!(state.user_bash_running());
    assert!(cancel.is_cancelled());
    state.finish_user_bash();
    assert!(!state.user_bash_running());
    // Nothing to cancel once idle.
    assert!(!state.cancel_user_bash());
}

#[test]
fn key_repeat_is_dispatched_but_release_is_not() {
    assert!(should_dispatch_key(KeyEventKind::Press));
    assert!(should_dispatch_key(KeyEventKind::Repeat));
    assert!(!should_dispatch_key(KeyEventKind::Release));
}

#[test]
fn test_layout_renders_welcome_message() {
    let chat = Arc::new(Container::new());
    add_welcome_message(&chat);

    let scroll = Arc::new(ScrollView::new(
        chat.clone(),
        ScrollViewOptions {
            follow: FollowMode::End,
            primary: true,
            ..Default::default()
        },
    ));

    let editor = Arc::new(Editor::new(
        EditorOptions {
            padding_x: 1,
            ..Default::default()
        },
        EditorStyle::default(),
        Arc::new(rpi_tui::Keybindings::new()),
    ));
    let dock = Arc::new(Container::new());
    dock.add_child(editor);

    let footer = Arc::new(FooterComponent::new());

    let root = VStack::from_children(vec![
        StackChild::Entry(StackEntry::new(scroll.clone()).grow(1).min_size(1)),
        StackChild::Entry(StackEntry::new(dock)),
        StackChild::Entry(StackEntry::new(footer)),
    ]);

    let frame = rpi_tui::render_layout_frame(Arc::new(root), 80, 24);

    let all: String = frame.lines.join("\n");
    assert!(
        all.contains("rpi"),
        "Welcome message not found. Rendered: {}",
        all
    );
    assert!(
        all.contains("Type your message"),
        "Help text not found. Rendered: {}",
        all
    );
}

#[test]
fn test_chat_container_has_welcome_content() {
    let chat = Arc::new(Container::new());
    add_welcome_message_with_capabilities(
        &chat,
        &["read".into(), "bash".into(), "web_fetch".into()],
        &["rust-review".into(), "release".into()],
        &[
            "/project/.rpi/SYSTEM.md".into(),
            "/project/AGENTS.md".into(),
        ],
    );

    let lines = chat.render(80);
    let all: String = lines.join("\n");
    // Welcome title is "rpi" (accent bold) + "interactive TUI" (muted),
    // joined by an ANSI reset — strip ANSI before checking the substring.
    let plain = strip_ansi(&all);
    assert!(
        plain.contains("rpi"),
        "Welcome message not in chat container: {:?}",
        lines
    );
    assert!(plain.contains("Tools (3)"), "Tool count missing: {plain}");
    assert!(
        plain.contains("read · bash · web_fetch"),
        "Tool names missing: {plain}"
    );
    assert!(plain.contains("Skills (2)"), "Skill count missing: {plain}");
    assert!(plain.contains("Context (2)"));
    assert!(plain.contains("/project/.rpi/SYSTEM.md"));
    assert!(plain.contains("/project/AGENTS.md"));
    assert!(
        plain.contains("rust-review · release"),
        "Skill names missing: {plain}"
    );
}

#[test]
fn skills_panel_lists_and_documents_explicit_invocation() {
    let chat = Arc::new(Container::new());
    let resources = rpi_harness::types::AgentHarnessResources {
        skills: Some(vec![rpi_harness::types::Skill {
            name: "release".into(),
            description: "Publish the current workspace".into(),
            content: "# Release".into(),
            file_path: "/skills/release/SKILL.md".into(),
            disable_model_invocation: Some(true),
        }]),
        ..Default::default()
    };

    show_skills_panel(&chat, &resources);
    let plain = strip_ansi(&chat.render(100).join("\n"));
    assert!(plain.contains("release [hidden]"));
    assert!(plain.contains("Publish the current workspace"));
    assert!(plain.contains("/skills <name> [instructions]"));
}

#[test]
fn reload_updates_context_header_and_context_panel_without_stale_paths() {
    use rpi_harness::types::AgentHarnessResources;
    let chat = Arc::new(Container::new());
    let old = AgentHarnessResources {
        context_files: vec!["/old/SYSTEM.md".into()],
        ..Default::default()
    };
    let listing = add_welcome_message_with_capabilities(&chat, &[], &[], &old.context_files);
    let shared = Arc::new(Mutex::new(old));
    let fresh = AgentHarnessResources {
        context_files: vec!["/new/APPEND_SYSTEM.md".into(), "/new/CLAUDE.md".into()],
        ..Default::default()
    };
    update_resource_listing(&shared, Some(&listing), fresh.clone());
    let header = strip_ansi(&chat.render(120).join("\n"));
    assert!(header.contains("Context (2)"));
    assert!(header.contains("/new/APPEND_SYSTEM.md"));
    assert!(!header.contains("/old/SYSTEM.md"));
    assert_eq!(*shared.lock().unwrap(), fresh);
    let output = context_panel_lines(&shared.lock().unwrap()).join("\n");
    assert!(output.contains("/new/CLAUDE.md"));
    assert!(!output.contains("/old/SYSTEM.md"));

    update_resource_listing(&shared, Some(&listing), Default::default());
    assert!(!strip_ansi(&chat.render(120).join("\n")).contains("Context"));
    assert!(context_panel_lines(&shared.lock().unwrap())
        .join("\n")
        .contains("no context files loaded"));
}

#[test]
fn welcome_header_renders_the_brand_mark() {
    let chat = Arc::new(Container::new());
    add_welcome_message(&chat);
    let lines: Vec<String> = chat.render(80).iter().map(|l| strip_ansi(l)).collect();
    let all = lines.join("\n");

    // The three-bar mark: the bottom row carries all three bars, and the
    // wordmark sits on its baseline next to the middle bar.
    assert!(
        lines.iter().any(|l| l.contains("██ ██ ██")),
        "brand mark missing: {all}"
    );
    assert!(
        all.contains("rpi"),
        "the mark must carry the product name: {all}"
    );

    // Regression guard for the layout bug this replaced: `π` is East-Asian
    // Ambiguous — width 1 normally, width 2 under a CJK locale — which
    // walked the old box borders out of alignment depending on the user's
    // system. The crab is deliberately wide but unambiguous, and `crate::brand`
    // tests pin its width so the padding cannot silently drift.
    assert!(
        !all.contains('π'),
        "the header must not use east-asian-ambiguous glyphs: {all}"
    );
}

#[test]
fn update_notices_render_inside_the_transcript() {
    let chat = Arc::new(Container::new());
    let report = crate::updates::UpdateReport {
        notices: vec![crate::updates::UpdateNotice {
            name: "rpi".into(),
            current: "0.1.10".into(),
            latest: "0.1.11".into(),
            command: "rpi update".into(),
        }],
        warnings: vec![crate::updates::UpdateWarning {
            message: "The previously scheduled rpi update failed: access denied".into(),
            command: "rpi update".into(),
        }],
    };

    add_update_notices(&chat, &report);

    assert_eq!(chat.child_count(), 1);
    let plain = strip_ansi(&chat.render(80).join("\n"));
    assert!(plain.contains("Update Failed"), "{plain}");
    assert!(
        plain.contains("rpi update failed: access denied"),
        "{plain}"
    );
    assert!(plain.contains("Update Available"), "{plain}");
    assert!(plain.contains("New version 0.1.11 is available"), "{plain}");
    assert!(plain.contains("rpi update"), "{plain}");
}

#[test]
fn empty_update_report_does_not_add_transcript_content() {
    let chat = Arc::new(Container::new());

    add_update_notices(&chat, &crate::updates::UpdateReport::default());

    assert_eq!(chat.child_count(), 0);
}

#[test]
fn package_updates_render_without_an_rpi_update() {
    let chat = Arc::new(Container::new());
    let package_notice = crate::updates::UpdateNotice {
        name: "rpi-im-message".into(),
        current: "0.1.9".into(),
        latest: "0.1.10".into(),
        command: "rpi install rpi-im-message".into(),
    };

    for include_rpi in [false, true] {
        chat.clear();
        let mut report = crate::updates::UpdateReport {
            notices: vec![package_notice.clone()],
            warnings: Vec::new(),
        };
        if include_rpi {
            report.notices.push(crate::updates::UpdateNotice {
                name: "rpi".into(),
                current: "0.3.20".into(),
                latest: "0.3.21".into(),
                command: "rpi update".into(),
            });
        }

        add_update_notices(&chat, &report);

        assert_eq!(chat.child_count(), 1);
        let plain = strip_ansi(&chat.render(80).join("\n"));
        assert!(plain.contains("Package Update Available"), "{plain}");
        assert!(plain.contains("rpi-im-message 0.1.9 -> 0.1.10"), "{plain}");
        assert!(plain.contains(&package_notice.command), "{plain}");
        assert!(
            !plain.contains(&format!("{}.", package_notice.command)),
            "{plain}"
        );
        assert_eq!(plain.contains("rpi update"), include_rpi, "{plain}");
    }
}

#[test]
fn welcome_capabilities_show_empty_state() {
    let plain = strip_ansi(&welcome_capability_line("Skills", &[]));
    assert_eq!(plain, "Skills (0) none");
}

#[test]
fn completed_stream_reconciles_the_final_tail_before_detaching() {
    let component = Arc::new(AssistantMessageComponent::new(
        AssistantMessageOptions::default(),
    ));
    component.set_streaming(true);
    component.update_blocks(&[AssistantBlock::Text("partial response".into())]);

    let current = Arc::new(Mutex::new(Some(component.clone())));
    let cached = Mutex::new("partial response".to_string());
    let mut final_message = AssistantMessage::empty(rpi_ai::Api::Faux, "faux", "faux-model", 0);
    final_message.content = vec![Content::text(
        "partial response with the previously missing final tail",
    )];
    final_message.stop_reason = rpi_ai::types::StopReason::Stop;

    reconcile_streamed_assistant_completion(&current, &cached, Some(&final_message));

    assert!(current.lock().unwrap().is_none());
    assert_eq!(
        cached.lock().unwrap().as_str(),
        "partial response with the previously missing final tail"
    );
    let rendered = strip_ansi(&component.render(100).join("\n"));
    assert!(
        rendered.contains("previously missing final tail"),
        "{rendered}"
    );
}

/// Reproduction for "Tab 补全了但显示没刷新": after `accept_top_suggestion`
/// replaces the editor text, the NEXT rendered frame must show the
/// completed text (" /model " with the caret after it), not the old
/// prefix. Mirrors the real dock layout (autocomplete_container above the
/// bordered editor) and drives the same accept path the Tab handler uses.
#[test]
fn tab_accept_suggestion_reflects_in_next_render() {
    use rpi_tui::render_layout_frame;

    let editor = Arc::new(Editor::new(
        EditorOptions {
            padding_x: 1,
            ..Default::default()
        },
        EditorStyle::default(),
        Arc::new(rpi_tui::Keybindings::new()),
    ));
    editor.set_focused(true);
    let editor_container = Arc::new(Container::new());
    editor_container.add_child(editor.clone());
    let autocomplete_container = Arc::new(Container::new());
    let footer = Arc::new(rpi_tui::Text::new("FOOTER", 0, 0));
    let dock = Arc::new(VStack::from_children(vec![
        StackChild::Entry(StackEntry::new(autocomplete_container.clone())),
        StackChild::Entry(
            StackEntry::new(editor_container.clone())
                .shrink(0)
                .min_size(3),
        ),
        StackChild::Entry(StackEntry::new(footer)),
    ]));

    // Simulate the user typing "/mo" (the popup shows suggestions).
    let manager = AutocompleteManager::new();
    let mut combined = CombinedAutocompleteProvider::new();
    combined.add_provider(Arc::new(
        SlashCommandAutocompleteProvider::with_default_commands(),
    ));
    combined.add_provider(Arc::new(FilePathAutocompleteProvider::new()));
    manager.set_provider(Arc::new(combined));
    // Simulate typing "/mo" via the real insert path (advances the caret
    // by char length, like `handle_key` does).
    editor.insert("/mo");
    assert_eq!(editor.cursor_position(), (0, 3));

    let frame_before = render_layout_frame(dock.clone(), 80, 10);
    assert!(
        frame_before.lines.iter().any(|l| l.contains("/mo")),
        "precondition: editor shows the typed prefix. Frame rows:\n{}",
        frame_before
            .lines
            .iter()
            .map(|l| format!("  [{l}]"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // Tab: accept the top suggestion (the same code path as the key loop).
    let text = editor.get_text();
    let cursor = editor_cursor_offset(&editor, &text);
    let sugg = manager
        .get_suggestions(&text, cursor)
        .expect("slash suggestions for /mo");
    let top = sugg.items.first().expect("at least one suggestion");
    let start = sugg.start.min(text.len());
    let end = sugg.end.min(text.len());
    let mut replaced = String::new();
    replaced.push_str(&text[..start]);
    replaced.push_str(&top.text);
    replaced.push_str(&text[end..]);
    if top.insert_space && !replaced.ends_with('/') {
        replaced.push(' ');
    }
    let new_cursor = start + top.text.len();
    editor.set_text(&replaced);
    set_editor_cursor_offset(&editor, &replaced, new_cursor);
    autocomplete_container.clear();
    assert_eq!(editor.get_text(), "/model");

    // The next render MUST display the completed text.
    let frame_after = render_layout_frame(dock, 80, 10);
    let all: String = frame_after.lines.join("\n");
    assert!(
        all.contains("/model"),
        "completed text missing from next render. Got:\n{all}"
    );
    // The caret must sit AFTER the completed command (the snap_boundary
    // regression put it one char early: "/mode|l" with the final char
    // dangling past the caret).
    let editor_line = frame_after
        .lines
        .iter()
        .find(|l| l.contains("/model"))
        .expect("editor row with completed text");
    assert!(
        editor_line.contains(&format!("/model{}", rpi_tui::CURSOR_MARKER)),
        "caret must follow the full completed text. Got: {editor_line:?}"
    );
}

#[test]
fn multiline_autocomplete_preserves_row_and_column() {
    let editor = Arc::new(Editor::simple());
    editor.set_text("first\n/mo");
    editor.set_cursor(1, 3);

    let text = editor.get_text();
    assert_eq!(editor_cursor_offset(&editor, &text), 9);

    let replaced = "first\n/model";
    editor.set_text(replaced);
    set_editor_cursor_offset(&editor, replaced, 12);
    assert_eq!(editor.cursor_position(), (1, 6));
}

#[test]
fn test_slash_command_dispatch() {
    // The registry is the single source of truth for dispatch: `find(token)`
    // returns the command (by name or alias) whose `name()` is the canonical
    // form, or `None` for an unknown token. This replaces the old enum-based
    // `handle_slash_command` assertions with equivalent registry lookups.
    let registry = build_builtin_registry();

    // Helper: a token resolves to the command with this canonical name.
    let resolves_to = |token: &str, canonical: &str| {
        let found = registry.find(token).expect("{token} should resolve");
        assert_eq!(
            found.name(),
            canonical,
            "{token} resolved to {} (expected {canonical})",
            found.name()
        );
    };

    resolves_to("/help", "/help");
    resolves_to("/?", "/help"); // alias → canonical
    resolves_to("/clear", "/clear");
    resolves_to("/new", "/clear"); // alias
    resolves_to("/q", "/exit"); // alias
    resolves_to("/quit", "/exit"); // alias
    resolves_to("/version", "/version");
    resolves_to("/v", "/version"); // alias
    resolves_to("/changelog", "/changelog");
    resolves_to("/hotkeys", "/hotkeys");
    resolves_to("/model", "/model");
    resolves_to("/m", "/model"); // alias
    resolves_to("/provider", "/provider");
    resolves_to("/theme", "/theme");
    resolves_to("/session", "/session");
    resolves_to("/resume", "/session"); // alias
    resolves_to("/compact", "/compact");
    resolves_to("/copy", "/copy");
    resolves_to("/thinking", "/thinking");
    resolves_to("/think", "/thinking"); // alias
    resolves_to("/tools", "/tools");
    resolves_to("/images", "/images");
    resolves_to("/context", "/context");
    resolves_to("/skills", "/skills");
    // Out-of-v1-scope commands resolve to their own UnsupportedCommand entry.
    resolves_to("/settings", "/settings");
    resolves_to("/name", "/name");
    resolves_to("/export", "/export");

    // Unknown token → not found.
    assert!(registry.find("/nope").is_none(), "/nope should be unknown");
}

#[test]

fn test_registry_visible_entries_cover_dispatch() {
    // The autocomplete list is derived from the registry, so every visible
    // command the dispatcher recognizes must appear in it — by construction,
    // but this guards against a future command being registered with
    // `visible()` / a non-empty description that the builder drops.
    let registry = build_builtin_registry();
    let names: Vec<String> = registry
        .visible_entries()
        .iter()
        .map(|c| c.name.clone())
        .collect();
    for recognized in [
        "/help",
        "/clear",
        "/new",
        "/exit",
        "/quit",
        "/version",
        "/changelog",
        "/model",
        "/provider",
        "/session",
        "/theme",
        "/compact",
        "/copy",
        "/hotkeys",
        "/tools",
        "/images",
        "/thinking",
        "/usage",
        "/context",
        "/skills",
    ] {
        assert!(
            names.contains(&recognized.to_string()),
            "{recognized} missing from autocomplete list"
        );
    }
    // Hidden commands stay off the list.
    for hidden in ["/q", "/m", "/v", "/think", "/resume", "/?"] {
        assert!(
            !names.contains(&hidden.to_string()),
            "{hidden} should be hidden from autocomplete"
        );
    }
}

#[test]
fn test_agent_event_mapping_creates_assistant_and_tool() {
    // Synthetic AgentEvent sequence → UI mutations, exercised against the
    // real drain handler with a no-op TUI stand-in.
    use rpi_ai::types::{
        StopReason, TextContent, TextContentType, ThinkingContent, ThinkingContentType, ToolCall,
        ToolCallType, Usage,
    };

    let state = Arc::new(TuiState {
        history_page: history::HistoryUi::default(),
        session_dir: std::path::PathBuf::from("unused-session-root"),
        current_assistant: Arc::new(std::sync::Mutex::new(None)),
        tool_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        bash_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        hide_thinking: Arc::new(std::sync::Mutex::new(false)),
        tool_outputs_expanded: Arc::new(std::sync::Mutex::new(false)),
        show_terminal_progress: true,
        status: std::sync::Mutex::new(RunStatus::Idle),
        ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
        ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
        editor_text: rpi_extensions::EditorTextMailbox::new(),
        auto_send: std::sync::Mutex::new(None),
        last_editor_text: std::sync::Mutex::new(String::new()),
        programmatic_editor_write: std::sync::atomic::AtomicBool::new(false),
        js_preparation_cancel: std::sync::Mutex::new(None),
        user_bash_cancel: std::sync::Mutex::new(None),
        pending_bash_messages: std::sync::Mutex::new(Vec::new()),
        footer: Arc::new(FooterComponent::new()),
        status_container: Arc::new(Container::new()),
        chat_container: Arc::new(Container::new()),
        loader: Arc::new(Loader::new()),
        editor: Arc::new(Editor::simple()),
        last_assistant_text: std::sync::Mutex::new(String::new()),
        active_selector: std::sync::Mutex::new(None),
        provider_form: std::sync::Mutex::new(None),
        active_provider_input: std::sync::Mutex::new(None),
        provider_auth_flow_id: std::sync::atomic::AtomicU64::new(0),
        provider_auth_generation: std::sync::atomic::AtomicU64::new(0),
        provider_auth_cancel: std::sync::Mutex::new(None),
        active_provider_select: std::sync::Mutex::new(None),
        active_extension_editor: std::sync::Mutex::new(None),
        active_extension_input: std::sync::Mutex::new(None),
        active_extension_cancel: std::sync::Mutex::new(None),
        autocomplete: AutocompleteManager::new(),
        autocomplete_container: Arc::new(Container::new()),
        autocomplete_max_visible: 5,
        autocomplete_selection: std::sync::Mutex::new(0),
        autocomplete_selection_stamped_for: std::sync::Mutex::new(None),
        pending_images: std::sync::Mutex::new(Vec::new()),
        theme_manager: Arc::new(ThemeManager::new()),
        active_theme_spec: std::sync::Mutex::new(None),
        theme_watcher: std::sync::Mutex::new(None),
        theme_registry: std::sync::RwLock::new(crate::themes::ThemeRegistry::default()),
        tui: None,
        current_model_id: std::sync::Mutex::new(String::new()),
        show_images: std::sync::Mutex::new(true),
        cache_miss_notices: std::sync::Mutex::new(false),
        history: std::sync::Mutex::new(Vec::new()),
        history_index: std::sync::Mutex::new(-1),
        history_draft: std::sync::Mutex::new(None),
        cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
        events_lagged: std::sync::atomic::AtomicBool::new(false),
        scoped_edit: std::sync::Mutex::new(None),
        markdown_transformer: Arc::new(std::sync::Mutex::new(None)),
        extension_claimed_keys: Mutex::new(Default::default()),
        extension_session: Arc::new(std::sync::Mutex::new(
            rpi_extensions::ExtensionSession::none(),
        )),
        search: Arc::new(AltScreenSearch::new()),
        search_bar: Arc::new(SearchBar::new()),
        pending_container: Arc::new(Container::new()),
        dequeue_hint: String::new(),
        pending_snapshot: std::sync::Mutex::new(
            rpi_harness::agent_harness::QueuedMessages::default(),
        ),
    });

    // The drain handler takes `Arc<TuiAltScreen>`, which needs a real
    // terminal; instead, exercise the *mutation* half directly against a
    // captured chat container via a synthetic message-start event's data.
    let assistant = AssistantMessage {
        role: rpi_ai::types::AssistantRole,
        content: vec![
            Content::Thinking(ThinkingContent {
                kind: ThinkingContentType,
                thinking: "Reasoning about the reply.".into(),
                thinking_signature: None,
                redacted: false,
            }),
            Content::Text(TextContent {
                kind: TextContentType,
                text: "Hello.".into(),
                text_signature: None,
            }),
            Content::ToolCall(ToolCall {
                kind: ToolCallType,
                id: "tc1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "echo hi"}),
                thought_signature: None,
                namespace: None,
                recorded: None,
            }),
        ],
        api: rpi_ai::Api::AnthropicMessages,
        provider: "anthropic".into(),
        model: "claude-sonnet-5".into(),
        response_model: None,
        response_id: None,
        usage: Usage::zero(),
        stop_reason: StopReason::Stop,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    };

    // Manually apply the MessageStart assistant branch logic (mirrors the
    // drain handler, without needing a TuiAltScreen).
    let comp = Arc::new(AssistantMessageComponent::new(
        AssistantMessageOptions::default(),
    ));
    comp.set_streaming(true);
    comp.update_blocks(&assistant_blocks(&assistant));
    let chat = Arc::new(Container::new());
    chat.add_child(comp.clone());
    *state.current_assistant.lock().unwrap() = Some(comp);

    // Manually apply the MessageUpdate tool-call scan (mirrors drain).
    for c in &assistant.content {
        if let Content::ToolCall(tc) = c {
            let mut tools = state.tool_components.lock().unwrap();
            if !tools.contains_key(&tc.id) {
                let tc_comp = Arc::new(ToolExecutionComponent::new(
                    &tc.name,
                    &tc.arguments.to_string(),
                ));
                tc_comp.set_running();
                chat.add_child(tc_comp.clone());
                tools.insert(tc.id.clone(), tc_comp);
            }
        }
    }

    // Assert: the assistant component rendered the text + the thinking
    // block (the update_blocks path keeps thinking visible), and a tool
    // component was registered.
    let rendered = chat.render(80);
    let joined: String = rendered.join("\n");
    assert!(
        joined.contains("Hello."),
        "assistant text not rendered: {joined}"
    );
    assert!(
        joined.contains("Reasoning about the reply."),
        "thinking block not rendered: {joined}"
    );
    assert_eq!(state.tool_components.lock().unwrap().len(), 1);
    assert!(state.current_assistant.lock().unwrap().is_some());

    // Manually apply ToolExecutionEnd (mirrors drain).
    let ended = state.tool_components.lock().unwrap().remove("tc1").unwrap();
    ended.set_result("hi", false);
    assert!(state.tool_components.lock().unwrap().is_empty());

    // A running bash panel owns the visible spinner. The global loader is
    // hidden until the last concurrent bash tool completes, then restored
    // while the agent remains in the Working state.
    assert!(state.try_start_working());
    assert!(
        !state.try_start_working(),
        "a second submit must be rejected"
    );
    state.set_status(RunStatus::Idle);
    state.set_status(RunStatus::Working);
    // The active loader now renders inside the editor's top border.
    assert!(state.editor.working().is_some());
    assert_eq!(state.status_container.child_count(), 0);
    assert!(state.footer.get_status().is_empty());
    state.show_retry(3, 10, 8_000);
    let retry_status = strip_ansi(&state.status_container.render(80).join("\n"));
    assert!(retry_status.contains("Retrying (3/10)"));
    state.set_status(RunStatus::Working);
    {
        let mut bash = state.bash_components.lock().unwrap();
        bash.insert(
            "bash-1".into(),
            Arc::new(BashExecutionComponent::new("one")),
        );
        bash.insert(
            "bash-2".into(),
            Arc::new(BashExecutionComponent::new("two")),
        );
    }
    state.sync_working_loader_with_bash();
    assert_eq!(state.status_container.child_count(), 0);
    state.bash_components.lock().unwrap().remove("bash-1");
    state.sync_working_loader_with_bash();
    assert_eq!(state.status_container.child_count(), 0);
    state.bash_components.lock().unwrap().remove("bash-2");
    state.sync_working_loader_with_bash();
    // The editor-border indicator stays visible across bash panels, so the
    // status container stays empty.
    assert_eq!(state.status_container.child_count(), 0);
    assert!(state.editor.working().is_some());

    state.set_status(RunStatus::Aborting);
    assert_eq!(state.status_container.child_count(), 0);
    assert!(!state.loader.is_running());
}

#[test]
fn fresh_launch_does_not_restore_old_history() {
    let fresh = Args::default();
    assert!(!launch_restores_history(&fresh));

    let continued = Args {
        continue_session: true,
        ..Args::default()
    };
    assert!(launch_restores_history(&continued));

    let selected = Args {
        session: Some("session-id".into()),
        ..Args::default()
    };
    assert!(launch_restores_history(&selected));
}

#[test]
fn context_badge_ignores_aborted_and_error_turns() {
    use rpi_ai::types::{Api, StopReason, Usage};

    let footer = Arc::new(FooterComponent::new());
    footer.set_context_window(100_000);

    // A real response fills in the percent (`?` -> `25.0%/100k`).
    let mut ok = rpi_ai::AssistantMessage::empty(Api::AnthropicMessages, "anthropic", "m", 0);
    ok.stop_reason = StopReason::Stop;
    ok.usage = Usage {
        input: 25_000,
        ..Usage::zero()
    };
    record_context_usage(&footer, &ok);
    let stats = strip_ansi(&footer.render(120).join("\n"));
    assert!(stats.contains("25.0%/100k"), "stats: {stats}");

    // An aborted turn must not clobber the last good value.
    let mut aborted = rpi_ai::AssistantMessage::empty(Api::AnthropicMessages, "anthropic", "m", 0);
    aborted.stop_reason = StopReason::Aborted;
    aborted.usage = Usage {
        input: 90_000,
        ..Usage::zero()
    };
    record_context_usage(&footer, &aborted);
    let stats = strip_ansi(&footer.render(120).join("\n"));
    assert!(stats.contains("25.0%/100k"), "stats: {stats}");

    // An errored turn likewise (its usage is not a real measurement).
    let mut errored = rpi_ai::AssistantMessage::empty(Api::AnthropicMessages, "anthropic", "m", 0);
    errored.stop_reason = StopReason::Error;
    errored.usage = Usage {
        input: 90_000,
        ..Usage::zero()
    };
    record_context_usage(&footer, &errored);
    let stats = strip_ansi(&footer.render(120).join("\n"));
    assert!(stats.contains("25.0%/100k"), "stats: {stats}");
}

#[test]
fn test_short_model_name() {
    assert_eq!(
        short_model_name("anthropic:claude-sonnet-5"),
        "claude-sonnet-5"
    );
    assert_eq!(short_model_name("claude-sonnet-5"), "claude-sonnet-5");
}

#[test]
fn model_selector_items_are_deduplicated_and_provider_qualified() {
    use rpi_ai::{Api, Model};

    let mut gateway = Model::new(
        "gpt-5.6-sol",
        "GPT 5.6 Sol",
        Api::OpenaiCompletions,
        "routeryo-copy",
        "https://gateway.example.com",
    );
    let duplicate = gateway.clone();
    let anthropic = Model::new(
        "claude-sonnet-5",
        "Claude Sonnet 5",
        Api::AnthropicMessages,
        "anthropic",
        "https://api.anthropic.com",
    );
    gateway.headers = Some(std::collections::BTreeMap::from([(
        "authorization".into(),
        "Bearer test".into(),
    )]));

    let items = model_selector_items(&[gateway, duplicate, anthropic], "gpt-5.6-sol");
    assert_eq!(items.len(), 2);
    // Value is provider-qualified so a bare id shared across endpoints resolves
    // to the right provider.
    assert_eq!(items[0].value, "routeryo-copy/gpt-5.6-sol");
    assert_eq!(items[0].label, "GPT 5.6 Sol");
    assert_eq!(
        items[0].description.as_deref(),
        Some("routeryo-copy/gpt-5.6-sol (current)")
    );
    assert_eq!(
        items[1].description.as_deref(),
        Some("anthropic/claude-sonnet-5")
    );
}

#[test]
fn model_selector_search_text_covers_provider_name_and_qualified_id() {
    use rpi_ai::{Api, Model};

    let anthropic = Model::new(
        "claude-sonnet-5",
        "Claude Sonnet 5",
        Api::AnthropicMessages,
        "anthropic",
        "https://api.anthropic.com",
    );
    let items = model_selector_items(&[anthropic], "");
    let search = items[0].search_key();
    // Mirrors native `getModelSelectorSearchText`: provider-prefixed first,
    // then the display name.
    assert!(search.starts_with("anthropic anthropic/claude-sonnet-5"));
    assert!(search.contains("Claude Sonnet 5"), "{search}");

    // A provider query and a display-name query both match through the
    // real fuzzy filter used by the searchable selector.
    use rpi_tui::fuzzy_filter;
    for query in ["anthropic", "sonnet", "Claude Sonnet", "claude-sonnet-5"] {
        let matched = fuzzy_filter(&items, query, |item| item.search_key());
        assert_eq!(matched.len(), 1, "query {query:?} matched nothing");
    }
}

#[test]
fn model_selector_match_accepts_bare_and_qualified_ids() {
    use rpi_ai::{Api, Model};

    let gateway = Model::new(
        "gpt-5.6-sol",
        "GPT 5.6 Sol",
        Api::OpenaiCompletions,
        "routeryo-copy",
        "https://gateway.example.com",
    );
    let anthropic = Model::new(
        "claude-sonnet-5",
        "Claude Sonnet 5",
        Api::AnthropicMessages,
        "anthropic",
        "https://api.anthropic.com",
    );
    let catalog = [gateway, anthropic];
    assert_eq!(
        find_model_selector_match(&catalog, "gpt-5.6-sol")
            .unwrap()
            .provider,
        "routeryo-copy"
    );
    assert_eq!(
        find_model_selector_match(&catalog, "routeryo-copy/gpt-5.6-sol")
            .unwrap()
            .id,
        "gpt-5.6-sol"
    );
    assert_eq!(
        find_model_selector_match(&catalog, "anthropic/claude-sonnet-5")
            .unwrap()
            .id,
        "claude-sonnet-5"
    );
    assert!(find_model_selector_match(&catalog, "other/gpt-5.6-sol").is_none());
}

#[test]
fn provider_input_canonicalization_exact_then_case_insensitive_unique() {
    use rpi_ai::{Api, Model};

    let anthropic = Model::new(
        "claude-sonnet-5",
        "Claude Sonnet 5",
        Api::AnthropicMessages,
        "anthropic",
        "https://api.anthropic.com",
    );
    let gateway = Model::new(
        "gpt-5.6-sol",
        "GPT 5.6 Sol",
        Api::OpenaiCompletions,
        "MyGateway",
        "https://gateway.example.com",
    );
    let catalog = [anthropic, gateway];

    assert_eq!(
        canonicalize_provider_input("anthropic", &catalog).unwrap(),
        "anthropic"
    );
    assert_eq!(
        canonicalize_provider_input("ANTHROPIC", &catalog).unwrap(),
        "anthropic"
    );
    assert_eq!(
        canonicalize_provider_input("mygateway", &catalog).unwrap(),
        "MyGateway"
    );
    assert!(canonicalize_provider_input("nope", &catalog).is_err());
}

#[test]
fn provider_login_callbacks_release_the_input_slot_before_submit_and_escape() {
    let state = test_tui_state();
    assert!(!handle_provider_login_key(
        &state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    ));
    let input = Arc::new(Input::new());
    let state_submit = state.clone();
    input.on_submit(Arc::new(move |text| {
        assert_eq!(text, "x");
        // Fail rather than hanging on a recursive lock if routing regresses.
        *state_submit
            .active_provider_input
            .try_lock()
            .expect("provider input slot locked during submit") = Some(Arc::new(Input::new()));
    }));
    let state_cancel = state.clone();
    input.on_escape(Arc::new(move || {
        *state_cancel
            .active_provider_input
            .try_lock()
            .expect("provider input slot locked during escape") = None;
    }));
    *state.active_provider_input.lock().unwrap() = Some(input.clone());
    assert!(handle_provider_login_key(
        &state,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)
    ));
    assert!(handle_provider_login_key(
        &state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    ));
    let replacement = state.active_provider_input.lock().unwrap().clone().unwrap();
    assert!(!Arc::ptr_eq(&input, &replacement));
    *state.active_provider_input.lock().unwrap() = Some(input);
    assert!(handle_provider_login_key(
        &state,
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    ));
    assert!(state.active_provider_input.lock().unwrap().is_none());
}

#[test]
fn provider_login_saves_deepseek_key_and_restores_editor() {
    with_isolated_config_dir(|| {
        let state = test_tui_state();
        let editor_container = Arc::new(Container::new());
        let editor = state.editor.clone();
        editor_container.add_child(editor.clone());
        let tui = Arc::new(TuiAltScreen::new(
            Box::new(ProcessTerminal::new()),
            true,
            None,
        ));
        let (tx, mut rx) = mpsc::unbounded_channel();
        begin_provider_login(
            &state,
            &editor_container,
            &editor,
            &tui,
            &tx,
            &state.extension_session,
            "deepseek",
        );
        assert!(!editor.is_focused());
        let input = state.active_provider_input.lock().unwrap().clone().unwrap();
        input.handle_paste("fixture-deepseek-key");
        assert!(handle_provider_login_key(
            &state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
        ));
        assert!(matches!(
            crate::config::read_auth().unwrap().get("deepseek"),
            Some(crate::config::Credential::ApiKey { key: Some(key), .. })
                if key == "fixture-deepseek-key"
        ));
        assert!(matches!(rx.try_recv(), Ok(TuiMessage::ReloadExtensions)));
        assert!(state.active_provider_input.lock().unwrap().is_none());
        assert!(editor.is_focused());
        assert_eq!(editor_container.child_count(), 1);

        // Esc closes the same login without saving or requesting another reload.
        begin_provider_login(
            &state,
            &editor_container,
            &editor,
            &tui,
            &tx,
            &state.extension_session,
            "cancelled-provider",
        );
        assert!(handle_provider_login_key(
            &state,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
        ));
        assert!(state.active_provider_input.lock().unwrap().is_none());
        assert!(editor.is_focused());
        assert!(!crate::config::read_auth()
            .unwrap()
            .contains_key("cancelled-provider"));
        assert!(rx.try_recv().is_err());
    });
}

/// A login cancelled with Esc while its exchange ran must not save the
/// credential that exchange returns late, and a newer flow does not adopt it.
#[test]
fn a_cancelled_provider_login_never_saves_a_late_credential() {
    with_isolated_config_dir(|| {
        let state = test_tui_state();
        let editor_container = Arc::new(Container::new());
        let editor = state.editor.clone();
        editor_container.add_child(editor.clone());
        let tui = Arc::new(TuiAltScreen::new(
            Box::new(ProcessTerminal::new()),
            true,
            None,
        ));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let late = || {
            Ok(serde_json::json!({
                "version": 1,
                "kind": "credential",
                "payload": {"access": "late-access", "refresh": "late-refresh", "expires_at": 0}
            }))
        };

        begin_provider_oauth_login(&state, &editor_container, &editor, &tui, &tx, "acme");
        let Ok(TuiMessage::ProviderOAuthAction {
            flow_id: cancelled, ..
        }) = rx.try_recv()
        else {
            panic!("begin was not sent");
        };
        finish_provider_login(&state, &editor_container, &editor, &tui);
        handle_provider_oauth_result(
            &state,
            &editor_container,
            &editor,
            &tui,
            &tx,
            "acme",
            "exchange",
            late(),
            ProviderOAuthPurpose::Login,
            cancelled,
        );
        assert!(crate::config::read_oauth_token("acme").unwrap().is_none());

        begin_provider_oauth_login(&state, &editor_container, &editor, &tui, &tx, "acme");
        handle_provider_oauth_result(
            &state,
            &editor_container,
            &editor,
            &tui,
            &tx,
            "acme",
            "exchange",
            late(),
            ProviderOAuthPurpose::Login,
            cancelled,
        );
        assert!(
            crate::config::read_oauth_token("acme").unwrap().is_none(),
            "a newer flow saved the cancelled flow's credential"
        );
        assert!(
            rx.try_recv().is_ok(),
            "the newer flow's own begin is still sent"
        );
        assert!(rx.try_recv().is_err(), "the stale result must not act");
    });
}

/// Esc ends an OAuth flow through `finish_provider_login`, which cancels the
/// call the flow started. The begin call and the exchange each carry the flow's
/// token, so Esc reaches whichever call is running.
#[test]
fn provider_oauth_escape_cancels_the_in_flight_call() {
    let state = test_tui_state();
    let editor_container = Arc::new(Container::new());
    let editor = state.editor.clone();
    editor_container.add_child(editor.clone());
    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        true,
        None,
    ));
    let (tx, mut rx) = mpsc::unbounded_channel();

    // Esc while `begin` is running.
    begin_provider_oauth_login(&state, &editor_container, &editor, &tui, &tx, "acme");
    let Ok(TuiMessage::ProviderOAuthAction {
        action,
        cancel: begin_cancel,
        ..
    }) = rx.try_recv()
    else {
        panic!("begin was not sent");
    };
    assert_eq!(action, "begin");
    assert!(!begin_cancel.is_cancelled());
    finish_provider_login(&state, &editor_container, &editor, &tui);
    assert!(begin_cancel.is_cancelled());

    // Esc while `exchange` is running: submit from the input stage, then Esc.
    begin_provider_oauth_login(&state, &editor_container, &editor, &tui, &tx, "acme");
    let Ok(TuiMessage::ProviderOAuthAction {
        cancel: second_begin_cancel,
        flow_id,
        ..
    }) = rx.try_recv()
    else {
        panic!("second begin was not sent");
    };
    assert!(
        !second_begin_cancel.is_cancelled(),
        "a new flow must not inherit the cancelled token"
    );
    let payload = serde_json::json!({
        "display": {"title": "Authorize", "url": "https://example.test/auth"}
    });
    begin_provider_oauth_input(
        &state,
        &editor_container,
        &editor,
        &tui,
        &tx,
        "acme",
        payload,
        flow_id,
    );
    let input = state.active_provider_input.lock().unwrap().clone().unwrap();
    input.handle_paste("code-value");
    assert!(handle_provider_login_key(
        &state,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
    ));
    let Ok(TuiMessage::ProviderOAuthAction {
        action,
        cancel: exchange_cancel,
        ..
    }) = rx.try_recv()
    else {
        panic!("exchange was not sent");
    };
    assert_eq!(action, "exchange");
    assert!(!exchange_cancel.is_cancelled());
    finish_provider_login(&state, &editor_container, &editor, &tui);
    assert!(exchange_cancel.is_cancelled());
    // After the flow ends, a stray submit gets an already-cancelled token, and
    // so does one from that flow after a newer flow began.
    assert!(state.provider_auth_token(flow_id).is_cancelled());
    let (newer, newer_cancel) = state.begin_provider_auth();
    assert!(state.provider_auth_token(flow_id).is_cancelled());
    assert!(!state.provider_auth_token(newer).is_cancelled());
    assert!(!newer_cancel.is_cancelled());
}

/// Esc during logout initialization: one Esc lands after the flow began but
/// before its status screen showed, which sends no revoke and leaves the editor
/// usable; one lands on the status screen, which cancels the queued revoke so a
/// late success for that flow leaves the stored credential alone.
#[test]
fn escape_during_logout_initialization_never_revokes_or_sticks() {
    with_isolated_config_dir(|| {
        let state = test_tui_state();
        let editor_container = Arc::new(Container::new());
        let editor = state.editor.clone();
        editor_container.add_child(editor.clone());
        let tui = Arc::new(TuiAltScreen::new(
            Box::new(ProcessTerminal::new()),
            true,
            None,
        ));
        let (tx, mut rx) = mpsc::unbounded_channel();
        crate::config::upsert_oauth_token("acme", "kept".into(), "kept".into(), 0).unwrap();
        let logout = || {
            begin_provider_oauth_logout(
                &state,
                &editor_container,
                &editor,
                &tui,
                &tx,
                "acme",
                "kept".into(),
                "kept".into(),
            )
        };

        // Esc between the flow start and the status screen.
        let escape_won = Arc::new(std::sync::atomic::AtomicBool::new(false));
        settings::AFTER_PROVIDER_FLOW_BEGIN.with(|hook| {
            let (state, editor_container, editor, tui, escape_won) = (
                state.clone(),
                editor_container.clone(),
                editor.clone(),
                tui.clone(),
                escape_won.clone(),
            );
            hook.set(Some(Box::new(move || {
                let won = finish_provider_login(&state, &editor_container, &editor, &tui);
                escape_won.store(won, std::sync::atomic::Ordering::SeqCst);
            })))
        });
        logout();
        assert!(
            escape_won.load(std::sync::atomic::Ordering::SeqCst),
            "the flow was not live when Esc landed"
        );
        assert!(rx.try_recv().is_err(), "a cancelled logout sent a revoke");
        assert!(editor.is_focused(), "the status screen covered the editor");
        assert_eq!(
            state
                .provider_auth_flow_id
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );

        // Esc on the status screen, before the revoke is dispatched.
        logout();
        let Ok(TuiMessage::ProviderOAuthAction {
            action,
            purpose: ProviderOAuthPurpose::Logout,
            flow_id,
            cancel,
            ..
        }) = rx.try_recv()
        else {
            panic!("revoke was not sent");
        };
        assert_eq!(action, "revoke");
        assert!(!cancel.is_cancelled());
        assert!(finish_provider_login(
            &state,
            &editor_container,
            &editor,
            &tui
        ));
        assert!(
            cancel.is_cancelled(),
            "the queued revoke must observe the cancel"
        );
        handle_provider_oauth_result(
            &state,
            &editor_container,
            &editor,
            &tui,
            &tx,
            "acme",
            "revoke",
            Ok(serde_json::json!({"version": 1, "kind": "success", "payload": {}})),
            ProviderOAuthPurpose::Logout,
            flow_id,
        );
        assert!(crate::config::read_oauth_token("acme").unwrap().is_some());
        assert!(rx.try_recv().is_err());
    });
}

/// An Esc during logout initialization followed at once by a new login leaves
/// that login running: the logout's late screen check neither cancels it nor
/// sends a revoke.
#[test]
fn a_late_logout_screen_check_leaves_a_newer_flow_running() {
    let state = test_tui_state();
    let editor_container = Arc::new(Container::new());
    let editor = state.editor.clone();
    editor_container.add_child(editor.clone());
    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        true,
        None,
    ));
    let (tx, mut rx) = mpsc::unbounded_channel();
    settings::AFTER_PROVIDER_FLOW_BEGIN.with(|hook| {
        let (state, editor_container, editor, tui, tx) = (
            state.clone(),
            editor_container.clone(),
            editor.clone(),
            tui.clone(),
            tx.clone(),
        );
        hook.set(Some(Box::new(move || {
            assert!(finish_provider_login(
                &state,
                &editor_container,
                &editor,
                &tui
            ));
            begin_provider_oauth_login(&state, &editor_container, &editor, &tui, &tx, "acme");
        })))
    });
    begin_provider_oauth_logout(
        &state,
        &editor_container,
        &editor,
        &tui,
        &tx,
        "acme",
        "kept".into(),
        "kept".into(),
    );
    let Ok(TuiMessage::ProviderOAuthAction {
        action,
        flow_id,
        cancel,
        ..
    }) = rx.try_recv()
    else {
        panic!("the newer login's begin was not sent");
    };
    assert_eq!(action, "begin");
    assert!(rx.try_recv().is_err(), "the cancelled logout sent a revoke");
    assert!(!cancel.is_cancelled(), "the newer login was cancelled");
    assert_eq!(
        state
            .provider_auth_flow_id
            .load(std::sync::atomic::Ordering::Acquire),
        flow_id
    );
}

/// A flow's id is published under the token lock, together with its token, so
/// a reader holding that lock (as Esc does) never sees a flow without its token.
#[test]
fn provider_auth_flow_id_is_published_with_its_token() {
    let state = test_tui_state();
    let slot = state.provider_auth_cancel.lock().unwrap();
    let begin = std::thread::spawn({
        let state = state.clone();
        move || state.begin_provider_auth()
    });
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(
        state
            .provider_auth_flow_id
            .load(std::sync::atomic::Ordering::Acquire),
        0,
        "the flow id was published before its token"
    );
    drop(slot);
    let (flow_id, cancel) = begin.join().unwrap();
    assert_eq!(
        state
            .provider_auth_flow_id
            .load(std::sync::atomic::Ordering::Acquire),
        flow_id
    );
    assert!(state.end_provider_auth());
    assert!(cancel.is_cancelled());
}

/// An interaction result whose flow Esc ended while the form was being shown
/// leaves the editor usable instead of a form no Esc can dismiss.
#[test]
fn an_interaction_form_for_an_ended_flow_restores_the_editor() {
    let state = test_tui_state();
    let editor_container = Arc::new(Container::new());
    let editor = state.editor.clone();
    editor_container.add_child(editor.clone());
    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        true,
        None,
    ));
    let (tx, mut rx) = mpsc::unbounded_channel();
    begin_provider_oauth_login(&state, &editor_container, &editor, &tui, &tx, "acme");
    let Ok(TuiMessage::ProviderOAuthAction { flow_id, .. }) = rx.try_recv() else {
        panic!("begin was not sent");
    };
    assert!(finish_provider_login(
        &state,
        &editor_container,
        &editor,
        &tui
    ));
    begin_provider_oauth_input(
        &state,
        &editor_container,
        &editor,
        &tui,
        &tx,
        "acme",
        serde_json::json!({"display": {"title": "Authorize"}}),
        flow_id,
    );
    assert!(state.active_provider_input.lock().unwrap().is_none());
    assert!(editor.is_focused());
    assert!(rx.try_recv().is_err());
}

/// A failed result that lost its flow to Esc, after which the user began a
/// newer flow, neither ends that newer flow nor reports an error for it.
#[test]
fn a_stale_failed_result_leaves_a_newer_flow_running() {
    let state = test_tui_state();
    let editor_container = Arc::new(Container::new());
    let editor = state.editor.clone();
    editor_container.add_child(editor.clone());
    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        true,
        None,
    ));
    let (tx, mut rx) = mpsc::unbounded_channel();
    begin_provider_oauth_login(&state, &editor_container, &editor, &tui, &tx, "acme");
    let Ok(TuiMessage::ProviderOAuthAction { flow_id: stale, .. }) = rx.try_recv() else {
        panic!("begin was not sent");
    };
    let newer = Arc::new(std::sync::Mutex::new(None));
    settings::BEFORE_PROVIDER_FLOW_CLAIM.with(|hook| {
        let (state, editor_container, editor, tui, tx, newer) = (
            state.clone(),
            editor_container.clone(),
            editor.clone(),
            tui.clone(),
            tx.clone(),
            newer.clone(),
        );
        hook.set(Some(Box::new(move || {
            finish_provider_login(&state, &editor_container, &editor, &tui);
            begin_provider_oauth_login(&state, &editor_container, &editor, &tui, &tx, "acme");
            *newer.lock().unwrap() = Some(
                state.provider_auth_token(
                    state
                        .provider_auth_flow_id
                        .load(std::sync::atomic::Ordering::Acquire),
                ),
            );
        })))
    });
    let messages_before = state.chat_container.child_count();
    handle_provider_oauth_result(
        &state,
        &editor_container,
        &editor,
        &tui,
        &tx,
        "acme",
        "begin",
        Err("network down".to_string()),
        ProviderOAuthPurpose::Login,
        stale,
    );
    let newer = newer.lock().unwrap().take().expect("Esc did not land");
    assert!(
        !newer.is_cancelled(),
        "the stale result cancelled the newer flow"
    );
    assert_ne!(
        state
            .provider_auth_flow_id
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
    assert_eq!(
        state.chat_container.child_count(),
        messages_before,
        "the stale result reported an error"
    );
}

/// Esc that lands after a result passed its flow check, but before the write,
/// wins the flow: neither the login credential nor the logout delete touches
/// the store. A completion that claims first leaves Esc nothing to cancel.
#[test]
fn escape_between_the_flow_check_and_the_write_keeps_the_store_unchanged() {
    with_isolated_config_dir(|| {
        let state = test_tui_state();
        let editor_container = Arc::new(Container::new());
        let editor = state.editor.clone();
        editor_container.add_child(editor.clone());
        let tui = Arc::new(TuiAltScreen::new(
            Box::new(ProcessTerminal::new()),
            true,
            None,
        ));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let escape_won = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let arm_escape = || {
            let (state, editor_container, editor, tui, escape_won) = (
                state.clone(),
                editor_container.clone(),
                editor.clone(),
                tui.clone(),
                escape_won.clone(),
            );
            settings::BEFORE_PROVIDER_FLOW_CLAIM.with(|hook| {
                hook.set(Some(Box::new(move || {
                    let won = finish_provider_login(&state, &editor_container, &editor, &tui);
                    escape_won.store(won, std::sync::atomic::Ordering::SeqCst);
                })))
            });
        };
        let credential = || {
            Ok(serde_json::json!({
                "version": 1,
                "kind": "credential",
                "payload": {"access": "late-access", "refresh": "late-refresh", "expires_at": 0}
            }))
        };
        let result = |flow_id, purpose, value| {
            handle_provider_oauth_result(
                &state,
                &editor_container,
                &editor,
                &tui,
                &tx,
                "acme",
                "exchange",
                value,
                purpose,
                flow_id,
            )
        };
        let begin = |rx: &mut mpsc::UnboundedReceiver<TuiMessage>| {
            begin_provider_oauth_login(&state, &editor_container, &editor, &tui, &tx, "acme");
            let Ok(TuiMessage::ProviderOAuthAction { flow_id, .. }) = rx.try_recv() else {
                panic!("begin was not sent");
            };
            flow_id
        };

        // Login: Esc lands between the check and the write.
        let flow_id = begin(&mut rx);
        arm_escape();
        result(flow_id, ProviderOAuthPurpose::Login, credential());
        assert!(escape_won.load(std::sync::atomic::Ordering::SeqCst));
        assert!(crate::config::read_oauth_token("acme").unwrap().is_none());
        assert!(rx.try_recv().is_err(), "a cancelled login must not reload");

        // Logout: the revoke succeeded, but Esc won, so the credential stays.
        crate::config::upsert_oauth_token("acme", "kept".into(), "kept".into(), 0).unwrap();
        let flow_id = begin(&mut rx);
        arm_escape();
        result(
            flow_id,
            ProviderOAuthPurpose::Logout,
            Ok(serde_json::json!({"version": 1, "kind": "success", "payload": {}})),
        );
        assert!(escape_won.load(std::sync::atomic::Ordering::SeqCst));
        assert!(crate::config::read_oauth_token("acme").unwrap().is_some());
        assert!(rx.try_recv().is_err());

        // The completion claims first: it saves, and a later Esc ends nothing.
        crate::config::delete_credential("acme").unwrap();
        let flow_id = begin(&mut rx);
        result(flow_id, ProviderOAuthPurpose::Login, credential());
        assert!(crate::config::read_oauth_token("acme").unwrap().is_some());
        assert!(!finish_provider_login(
            &state,
            &editor_container,
            &editor,
            &tui
        ));
    });
}

#[test]
fn provider_form_saves_provider_to_models_json() {
    with_isolated_config_dir(|| {
        let form = ProviderFormState {
            step: ProviderFormStep::ModelName,
            id: "openrouter".into(),
            api: "openai-completions".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            api_key: "$OPENROUTER_API_KEY".into(),
            model_id: "claude-sonnet".into(),
            model_name: "Claude Sonnet".into(),
        };
        let note = save_provider_from_form(&form).unwrap();
        assert!(note.contains("openrouter"), "{note}");

        let cfg = crate::config::load_models_config().unwrap();
        let provider = cfg.providers.get("openrouter").expect("provider written");
        assert_eq!(provider.api.as_deref(), Some("openai-completions"));
        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://openrouter.ai/api/v1")
        );
        assert_eq!(provider.api_key.as_deref(), Some("$OPENROUTER_API_KEY"));
        assert_eq!(provider.models.len(), 1);
        assert_eq!(provider.models[0].id, "claude-sonnet");
        assert_eq!(provider.models[0].name.as_deref(), Some("Claude Sonnet"));

        // The duplicate-id guard sees the provider we just wrote.
        assert!(provider_id_exists("openrouter"));
        assert!(!provider_id_exists("nope"));
    });
}

#[test]
fn provider_url_validation_requires_http_scheme() {
    assert!(is_valid_provider_url("https://api.example.com/v1"));
    assert!(is_valid_provider_url("http://localhost:8080"));
    assert!(!is_valid_provider_url("api.example.com/v1"));
    assert!(!is_valid_provider_url(""));
}

#[test]
fn remove_provider_deletes_from_models_json() {
    with_isolated_config_dir(|| {
        let form = ProviderFormState {
            step: ProviderFormStep::ModelName,
            id: "openrouter".into(),
            api: "openai-completions".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            api_key: "$OPENROUTER_API_KEY".into(),
            model_id: "claude-sonnet".into(),
            model_name: String::new(),
        };
        save_provider_from_form(&form).unwrap();
        assert!(provider_id_exists("openrouter"));

        let note = remove_provider("openrouter").unwrap();
        assert!(note.contains("openrouter"), "{note}");
        assert!(!provider_id_exists("openrouter"));

        // Built-ins / unknown ids cannot be removed.
        assert!(remove_provider("anthropic").is_err());
        assert!(remove_provider("nope").is_err());
    });
}

/// Isolate the config dir (and serialize against other env-touching tests)
/// while `body` runs, then restore the previous value.
fn with_isolated_config_dir<T>(body: impl FnOnce() -> T) -> T {
    let _guard = crate::config::test_support::env_lock().lock().unwrap();
    let prev = std::env::var_os(crate::config::CONFIG_DIR_ENV);
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var(crate::config::CONFIG_DIR_ENV, tmp.path());
    let result = body();
    match prev {
        Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
        None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
    }
    result
}

// Existing catalog/persistence cases use a real lane and an explicitly supplied
// available catalog. Separate cases below exercise missing auth and lane failure.
fn switch_provider_for_test(
    input: &str,
    manifests: &[rpi_extensions::ProviderManifest],
) -> Result<String, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let harness = AgentHarness::create(Default::default()).await.unwrap();
            let lane = harness.lane("main");
            let available = crate::provider::catalog_with_manifests(manifests).unwrap();
            let result = switch_provider(input, manifests, &available, &lane).await;
            if let Ok((model, _)) = &result {
                assert_eq!(lane.get_model().await.unwrap(), *model);
            }
            harness.close().await.unwrap();
            result.map(|(_, note)| note)
        })
}

#[test]
fn provider_selector_items_cover_builtin_providers() {
    with_isolated_config_dir(|| {
        let items = provider_selector_items(&[], None);
        let ids: Vec<&str> = items.iter().map(|item| item.value.as_str()).collect();
        assert!(
            ids.contains(&"anthropic"),
            "built-in anthropic provider missing: {ids:?}"
        );
        assert!(
            ids.contains(&"openai"),
            "built-in openai provider missing: {ids:?}"
        );
        // Every row advertises its model count, origin, and key status, so a
        // built-in catalog entry can't be mistaken for a configured one.
        assert!(items.iter().all(|item| {
            item.description
                .as_deref()
                .is_some_and(|d| d.contains("model(s)") && d.contains("内置") && d.contains("key"))
        }));
    });
}

#[test]
fn set_default_provider_persists_provider_and_a_matching_model() {
    with_isolated_config_dir(|| {
        let note = switch_provider_for_test("openai", &[]).unwrap();
        assert!(
            note.contains("openai"),
            "note should name the provider: {note}"
        );

        let settings = crate::settings::load_settings().unwrap();
        assert_eq!(settings.default_provider.as_deref(), Some("openai"));
        assert!(
            settings.default_model.is_some(),
            "a matching default model must be saved so the choice resolves on restart"
        );

        // Unknown provider → error, and nothing is persisted.
        assert!(switch_provider_for_test("does-not-exist", &[]).is_err());
    });
}

#[test]
fn provider_switch_applies_live_and_keeps_configured_protocol_for_duplicate_model_ids() {
    with_isolated_config_dir(|| {
        let cfg = serde_json::from_value(serde_json::json!({"providers":{"deepseek":{
            "api":"openai-responses", "baseUrl":"http://fixture.invalid/v1", "apiKey":"fixture",
            "models":[{"id":"deepseek-flash"}]
        }}}))
        .unwrap();
        crate::config::save_models_config(&cfg).unwrap();
        let manifest = command_package_manifest("deepseek", "deepseek-flash", true);
        let response_model = crate::provider::catalog_with_manifests(&[manifest.clone()])
            .unwrap()
            .into_iter()
            .find(|model| model.provider == "deepseek" && model.api == rpi_ai::Api::OpenaiResponses)
            .unwrap();
        let available = vec![manifest.models[0].clone(), response_model];
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let harness = AgentHarness::create(Default::default()).await.unwrap();
                let lane = harness.lane("main");
                let (model, note) = switch_provider("DEEPSEEK", &[manifest], &available, &lane)
                    .await
                    .unwrap();
                assert_eq!(model.provider, "deepseek");
                assert_eq!(model.api, rpi_ai::Api::OpenaiResponses);
                assert_eq!(lane.get_model().await.unwrap(), model);
                assert!(
                    note.contains("Provider switched to deepseek") && !note.contains("restart")
                );
                let saved = crate::settings::load_settings().unwrap();
                assert_eq!(saved.default_provider.as_deref(), Some("deepseek"));
                assert_eq!(saved.default_model.as_deref(), Some("deepseek-flash"));
                // A known but unavailable provider and an unknown id preserve the lane/default.
                for id in ["openai", "unknown-provider"] {
                    assert!(switch_provider(id, &[], &[], &lane).await.is_err());
                    assert_eq!(lane.get_model().await.unwrap(), model);
                    assert_eq!(
                        crate::settings::load_settings().unwrap().default_provider,
                        saved.default_provider
                    );
                }
                harness.close().await.unwrap();
                assert!(switch_provider("deepseek", &[], &available, &lane)
                    .await
                    .is_err());
                assert_eq!(
                    crate::settings::load_settings().unwrap().default_provider,
                    saved.default_provider
                );
            });
    });
}

#[test]
fn provider_selector_marks_the_live_provider_instead_of_the_saved_default() {
    with_isolated_config_dir(|| {
        let mut settings = crate::settings::Settings::default();
        settings.default_provider = Some("anthropic".into());
        crate::settings::save_settings(&settings).unwrap();
        let items = provider_selector_items(&[], Some("openai"));
        assert!(items
            .iter()
            .find(|item| item.value == "openai")
            .unwrap()
            .description
            .as_deref()
            .unwrap()
            .contains("(current)"));
        assert!(!items
            .iter()
            .find(|item| item.value == "anthropic")
            .unwrap()
            .description
            .as_deref()
            .unwrap()
            .contains("(current)"));
    });
}

fn command_package_manifest(
    id: &str,
    model: &str,
    requires_auth: bool,
) -> rpi_extensions::ProviderManifest {
    rpi_extensions::ProviderManifest {
        id: id.into(),
        models: vec![rpi_ai::Model::new(
            model,
            model,
            rpi_ai::Api::OpenaiCompletions,
            id,
            "http://localhost:8080/v1",
        )],
        api_key_env: Some("RPI_TEST_PROVIDER_COMMAND_KEY".into()),
        requires_auth,
    }
}

#[test]
fn login_provider_items_are_provider_scoped_and_exclude_anonymous_packages() {
    with_isolated_config_dir(|| {
        let manifests = vec![
            command_package_manifest("deepseek", "deepseek-flash", true),
            command_package_manifest("llama-cpp", "local", false),
        ];
        let items = login_provider_items(&manifests);
        let deepseek = items
            .iter()
            .find(|item| item.value == "deepseek")
            .expect("DeepSeek should be login-capable");
        assert!(deepseek
            .description
            .as_deref()
            .is_some_and(|text| text.contains("RPI_TEST_PROVIDER_COMMAND_KEY")));
        assert!(!items.iter().any(|item| item.value == "llama-cpp"));
        assert!(items.iter().any(|item| item.value == "anthropic"));
        assert!(items.iter().any(|item| item.value == "openai"));
    });
}
#[test]
fn login_provider_items_include_oauth_only_providers_without_anonymous_packages() {
    with_isolated_config_dir(|| {
        let manifests = vec![command_package_manifest("llama-cpp", "local", false)];
        let oauth = vec![rpi_extensions::OAuthProviderInfo {
            plugin: "oauth-plugin".into(),
            id: "acme-oauth".into(),
            manifest: serde_json::json!({"displayName": "Acme OAuth"})
                .as_object()
                .unwrap()
                .clone(),
        }];
        let items = login_provider_items_with_oauth(&manifests, &oauth);
        let acme = items
            .iter()
            .find(|item| item.value == "acme-oauth")
            .expect("OAuth-only provider should be login-capable");
        assert!(acme
            .description
            .as_deref()
            .is_some_and(|text| text.contains("OAuth 登录")));
        assert!(!items.iter().any(|item| item.value == "llama-cpp"));
    });
}

#[test]
fn provider_commands_include_packages_without_keys_and_save_their_model_identity() {
    with_isolated_config_dir(|| {
        let mut router = command_package_manifest("openrouter", "openrouter/auto", true);
        // An absent variable is deterministic and cannot inherit an OpenAI key.
        router.api_key_env = None;
        let manifests = vec![
            router,
            command_package_manifest("llama-cpp", "local", false),
        ];
        let items = provider_selector_items(&manifests, None);
        let router = items
            .iter()
            .find(|item| item.value == "openrouter")
            .unwrap();
        assert!(router.description.as_deref().unwrap().contains("package"));
        assert!(router
            .description
            .as_deref()
            .unwrap()
            .contains("未配置 key"));
        let local = items.iter().find(|item| item.value == "llama-cpp").unwrap();
        assert!(local.description.as_deref().unwrap().contains("无需 key"));
        switch_provider_for_test("OPENROUTER", &manifests).unwrap();
        let saved = crate::settings::load_settings().unwrap();
        assert_eq!(saved.default_provider.as_deref(), Some("openrouter"));
        assert_eq!(saved.default_model.as_deref(), Some("openrouter/auto"));

        // After unload, stale provider selections fail without replacing settings.
        assert!(switch_provider_for_test("openrouter", &[]).is_err());
        assert_eq!(
            crate::settings::load_settings().unwrap().default_provider,
            saved.default_provider
        );
        assert!(!provider_selector_items(&[], None)
            .iter()
            .any(|item| item.value == "openrouter"));
    });
}

#[test]
fn provider_commands_respect_package_model_overrides_and_config_removal() {
    with_isolated_config_dir(|| {
        let manifests = vec![command_package_manifest(
            "openrouter",
            "openrouter/auto",
            true,
        )];
        let cfg = serde_json::from_value(serde_json::json!({"providers": {"openrouter": {
            "baseUrl": "http://override.example/v1", "models": [{"id": "openrouter/auto", "name": "Override"}]
        }}})).unwrap();
        crate::config::save_models_config(&cfg).unwrap();
        let catalog = crate::provider::catalog_with_manifests(&manifests).unwrap();
        let models: Vec<_> = catalog
            .iter()
            .filter(|model| model.provider == "openrouter")
            .collect();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "Override");
        assert_eq!(models[0].base_url, "http://override.example/v1");
        switch_provider_for_test("openrouter", &manifests).unwrap();
        remove_provider("openrouter").unwrap();
        // Removing models.json configuration leaves the installed package available.
        assert!(provider_selector_items(&manifests, None)
            .iter()
            .any(|item| item.value == "openrouter"));
        switch_provider_for_test("openrouter", &manifests).unwrap();
    });
}

#[test]
fn package_catalog_resolves_header_references_without_binding_credentials() {
    with_isolated_config_dir(|| {
        let manifests = vec![command_package_manifest("package-gateway", "model", true)];
        let cfg = serde_json::from_value(serde_json::json!({"providers":{"package-gateway":{
            "apiKey":"secret-value", "headers":{"Authorization":"$RPI_TEST_MISSING_PACKAGE_HEADER"}, "models":[]
        }}})).unwrap();
        crate::config::save_models_config(&cfg).unwrap();
        let catalog = crate::provider::catalog_with_manifests(&manifests).unwrap();
        let model = catalog
            .iter()
            .find(|model| model.provider == "package-gateway")
            .unwrap();
        assert!(!crate::credentials::has_header_auth(model.headers.as_ref()));
    });
}

#[test]
fn provider_package_key_status_uses_its_declared_environment() {
    with_isolated_config_dir(|| {
        let manifest = command_package_manifest("openrouter", "openrouter/auto", true);
        let previous = std::env::var_os("RPI_TEST_PROVIDER_COMMAND_KEY");
        std::env::set_var("RPI_TEST_PROVIDER_COMMAND_KEY", "package-key");
        let items = provider_selector_items(&[manifest], None);
        match previous {
            Some(value) => std::env::set_var("RPI_TEST_PROVIDER_COMMAND_KEY", value),
            None => std::env::remove_var("RPI_TEST_PROVIDER_COMMAND_KEY"),
        }
        let router = items
            .iter()
            .find(|item| item.value == "openrouter")
            .unwrap();
        assert!(router
            .description
            .as_deref()
            .unwrap()
            .contains("已配置 key"));
    });
}

#[test]
fn model_selector_accepts_package_ids_with_slashes_without_hiding_ambiguity() {
    let router = command_package_manifest("openrouter", "openrouter/auto", true)
        .models
        .remove(0);
    assert_eq!(
        find_model_selector_match(&[router.clone()], "openrouter/auto")
            .unwrap()
            .id,
        "openrouter/auto"
    );
    assert_eq!(
        find_model_selector_match(&[router.clone()], "openrouter/openrouter/auto")
            .unwrap()
            .id,
        "openrouter/auto"
    );
    let mut other = router.clone();
    other.provider = "another-router".into();
    assert!(find_model_selector_match(&[router.clone(), other], "openrouter/auto").is_none());
    let qualified = command_package_manifest("openrouter", "auto", true)
        .models
        .remove(0);
    assert_eq!(
        find_model_selector_match(&[router, qualified], "openrouter/auto")
            .unwrap()
            .id,
        "auto"
    );
}

#[test]
fn assistant_error_text_keeps_terminal_provider_diagnostic_visible() {
    use rpi_ai::types::{AssistantMessage, AssistantRole, StopReason, Usage};

    let failed = AssistantMessage {
        role: AssistantRole,
        content: Vec::new(),
        api: rpi_ai::Api::AnthropicMessages,
        provider: "anthropic".into(),
        model: "claude-sonnet-5".into(),
        response_model: None,
        response_id: None,
        usage: Usage::zero(),
        stop_reason: StopReason::Error,
        deferred: None,
        error_message: Some("upstream returned 401".into()),
        raw_stop_reason: None,
        end_turn: None,
        timestamp: 0,
    };
    assert_eq!(
        assistant_error_text(&failed).as_deref(),
        Some("upstream returned 401")
    );

    let mut no_detail = failed;
    no_detail.error_message = Some("  ".into());
    assert_eq!(
        assistant_error_text(&no_detail).as_deref(),
        Some("Provider request failed.")
    );

    let mut aborted = no_detail;
    aborted.stop_reason = StopReason::Aborted;
    aborted.error_message = Some("abort error: Request aborted".into());
    assert_eq!(
        assistant_error_text(&aborted).as_deref(),
        Some("abort error: Request aborted")
    );

    aborted.error_message = None;
    assert_eq!(
        assistant_error_text(&aborted).as_deref(),
        Some("Request aborted.")
    );

    // A truncated turn keeps its text but still needs a visible reason: without
    // one the transcript looks like the message was interrupted.
    let mut truncated = aborted;
    truncated.stop_reason = StopReason::Length;
    truncated.content = vec![rpi_ai::types::Content::text("half an answer")];
    let note = assistant_error_text(&truncated).expect("length must be surfaced");
    assert!(note.contains("truncated"), "{note}");
    assert!(note.contains("max_tokens"), "{note}");
}

#[test]
fn sanitize_error_message_keeps_diagnostics_without_terminal_controls() {
    assert_eq!(
        sanitize_error_message("405\r\nMethod Not Allowed\x1b[2J"),
        "405\nMethod Not Allowed"
    );
    assert_eq!(sanitize_error_message("\0\tmessage"), "\tmessage");
    assert_eq!(sanitize_error_message("   "), "Provider request failed.");
    let long = "x".repeat(20_000);
    let cleaned = sanitize_error_message(&long);
    assert!(cleaned.chars().count() <= 16 * 1024 + 1);
    assert!(cleaned.ends_with('…'));
}

#[test]
fn test_cycle_next_model_wraps_around() {
    use rpi_ai::{Api, Model};
    let mk = |id: &str| {
        Model::new(
            id,
            id,
            Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        )
    };
    let catalog = [mk("a"), mk("b"), mk("c")];
    // Next after "a" is "b"; after "c" wraps to "a".
    assert_eq!(cycle_next_model(&catalog, &mk("a")).unwrap().id, "b");
    assert_eq!(cycle_next_model(&catalog, &mk("c")).unwrap().id, "a");
    // An unknown current id falls back to the first model.
    assert_eq!(cycle_next_model(&catalog, &mk("zzz")).unwrap().id, "a");
    // Empty catalog yields None.
    let empty: Vec<Model> = vec![];
    assert!(cycle_next_model(&empty, &mk("a")).is_none());
}

#[test]
fn test_cycle_prev_model_wraps_around() {
    use rpi_ai::{Api, Model};
    let mk = |id: &str| {
        Model::new(
            id,
            id,
            Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        )
    };
    let catalog = [mk("a"), mk("b"), mk("c")];
    // Previous before "b" is "a"; before "a" wraps to "c".
    assert_eq!(cycle_prev_model(&catalog, &mk("b")).unwrap().id, "a");
    assert_eq!(cycle_prev_model(&catalog, &mk("a")).unwrap().id, "c");
    // An unknown current id falls back to the first model.
    assert_eq!(cycle_prev_model(&catalog, &mk("zzz")).unwrap().id, "a");
    // Empty catalog yields None.
    let empty: Vec<Model> = vec![];
    assert!(cycle_prev_model(&empty, &mk("a")).is_none());
}

#[test]
fn test_autocomplete_slash_suggestions_render() {
    // The autocomplete container should render at least one suggestion
    // line when the editor holds a `/` prefix, and clear when it doesn't.
    let state = Arc::new(TuiState {
        history_page: history::HistoryUi::default(),
        session_dir: std::path::PathBuf::from("unused-session-root"),
        current_assistant: Arc::new(std::sync::Mutex::new(None)),
        tool_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        bash_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        hide_thinking: Arc::new(std::sync::Mutex::new(false)),
        tool_outputs_expanded: Arc::new(std::sync::Mutex::new(false)),
        show_terminal_progress: true,
        status: std::sync::Mutex::new(RunStatus::Idle),
        ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
        ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
        editor_text: rpi_extensions::EditorTextMailbox::new(),
        auto_send: std::sync::Mutex::new(None),
        last_editor_text: std::sync::Mutex::new(String::new()),
        programmatic_editor_write: std::sync::atomic::AtomicBool::new(false),
        js_preparation_cancel: std::sync::Mutex::new(None),
        user_bash_cancel: std::sync::Mutex::new(None),
        pending_bash_messages: std::sync::Mutex::new(Vec::new()),
        footer: Arc::new(FooterComponent::new()),
        status_container: Arc::new(Container::new()),
        chat_container: Arc::new(Container::new()),
        loader: Arc::new(Loader::new()),
        editor: Arc::new(Editor::simple()),
        last_assistant_text: std::sync::Mutex::new(String::new()),
        active_selector: std::sync::Mutex::new(None),
        provider_form: std::sync::Mutex::new(None),
        active_provider_input: std::sync::Mutex::new(None),
        provider_auth_flow_id: std::sync::atomic::AtomicU64::new(0),
        provider_auth_generation: std::sync::atomic::AtomicU64::new(0),
        provider_auth_cancel: std::sync::Mutex::new(None),
        active_provider_select: std::sync::Mutex::new(None),
        active_extension_editor: std::sync::Mutex::new(None),
        active_extension_input: std::sync::Mutex::new(None),
        active_extension_cancel: std::sync::Mutex::new(None),
        autocomplete: AutocompleteManager::new(),
        autocomplete_container: Arc::new(Container::new()),
        autocomplete_max_visible: 5,
        autocomplete_selection: std::sync::Mutex::new(0),
        autocomplete_selection_stamped_for: std::sync::Mutex::new(None),
        pending_images: std::sync::Mutex::new(Vec::new()),
        theme_manager: Arc::new(ThemeManager::new()),
        active_theme_spec: std::sync::Mutex::new(None),
        theme_watcher: std::sync::Mutex::new(None),
        theme_registry: std::sync::RwLock::new(crate::themes::ThemeRegistry::default()),
        tui: None,
        current_model_id: std::sync::Mutex::new(String::new()),
        show_images: std::sync::Mutex::new(true),
        cache_miss_notices: std::sync::Mutex::new(false),
        history: std::sync::Mutex::new(Vec::new()),
        history_index: std::sync::Mutex::new(-1),
        history_draft: std::sync::Mutex::new(None),
        cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
        events_lagged: std::sync::atomic::AtomicBool::new(false),
        scoped_edit: std::sync::Mutex::new(None),
        markdown_transformer: Arc::new(std::sync::Mutex::new(None)),
        extension_claimed_keys: Mutex::new(Default::default()),
        extension_session: Arc::new(std::sync::Mutex::new(
            rpi_extensions::ExtensionSession::none(),
        )),
        search: Arc::new(AltScreenSearch::new()),
        search_bar: Arc::new(SearchBar::new()),
        pending_container: Arc::new(Container::new()),
        dequeue_hint: String::new(),
        pending_snapshot: std::sync::Mutex::new(
            rpi_harness::agent_harness::QueuedMessages::default(),
        ),
    });
    {
        let mut combined = CombinedAutocompleteProvider::new();
        combined.add_provider(Arc::new(SlashCommandAutocompleteProvider::new(
            build_builtin_registry().visible_entries(),
        )));
        state.autocomplete.set_provider(Arc::new(combined));
    }

    let editor = Arc::new(Editor::simple());
    editor.set_text("/he");
    editor.set_cursor(0, 3);
    refresh_autocomplete(&state, &editor);
    let lines = state.autocomplete_container.render(80);
    let joined: String = lines.join("\n");
    assert!(
        joined.contains("/help"),
        "slash suggestions not rendered: {joined}"
    );

    // Clear: no suggestions for plain text.
    editor.set_text("hello");
    editor.set_cursor(0, 5);
    refresh_autocomplete(&state, &editor);
    assert!(state.autocomplete_container.render(80).is_empty());
}

#[test]
fn test_select_list_swap_restores_editor() {
    // The editor-container swap: opening a selector replaces the editor
    // child; closing restores it. Verify the container child count + the
    // active_selector flag round-trip.
    let state = Arc::new(TuiState {
        history_page: history::HistoryUi::default(),
        session_dir: std::path::PathBuf::from("unused-session-root"),
        current_assistant: Arc::new(std::sync::Mutex::new(None)),
        tool_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        bash_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        hide_thinking: Arc::new(std::sync::Mutex::new(false)),
        tool_outputs_expanded: Arc::new(std::sync::Mutex::new(false)),
        show_terminal_progress: true,
        status: std::sync::Mutex::new(RunStatus::Idle),
        ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
        ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
        editor_text: rpi_extensions::EditorTextMailbox::new(),
        auto_send: std::sync::Mutex::new(None),
        last_editor_text: std::sync::Mutex::new(String::new()),
        programmatic_editor_write: std::sync::atomic::AtomicBool::new(false),
        js_preparation_cancel: std::sync::Mutex::new(None),
        user_bash_cancel: std::sync::Mutex::new(None),
        pending_bash_messages: std::sync::Mutex::new(Vec::new()),
        footer: Arc::new(FooterComponent::new()),
        status_container: Arc::new(Container::new()),
        chat_container: Arc::new(Container::new()),
        loader: Arc::new(Loader::new()),
        editor: Arc::new(Editor::simple()),
        last_assistant_text: std::sync::Mutex::new(String::new()),
        active_selector: std::sync::Mutex::new(None),
        provider_form: std::sync::Mutex::new(None),
        active_provider_input: std::sync::Mutex::new(None),
        provider_auth_flow_id: std::sync::atomic::AtomicU64::new(0),
        provider_auth_generation: std::sync::atomic::AtomicU64::new(0),
        provider_auth_cancel: std::sync::Mutex::new(None),
        active_provider_select: std::sync::Mutex::new(None),
        active_extension_editor: std::sync::Mutex::new(None),
        active_extension_input: std::sync::Mutex::new(None),
        active_extension_cancel: std::sync::Mutex::new(None),
        autocomplete: AutocompleteManager::new(),
        autocomplete_container: Arc::new(Container::new()),
        autocomplete_max_visible: 5,
        autocomplete_selection: std::sync::Mutex::new(0),
        autocomplete_selection_stamped_for: std::sync::Mutex::new(None),
        pending_images: std::sync::Mutex::new(Vec::new()),
        theme_manager: Arc::new(ThemeManager::new()),
        active_theme_spec: std::sync::Mutex::new(None),
        theme_watcher: std::sync::Mutex::new(None),
        theme_registry: std::sync::RwLock::new(crate::themes::ThemeRegistry::default()),
        tui: None,
        current_model_id: std::sync::Mutex::new(String::new()),
        show_images: std::sync::Mutex::new(true),
        cache_miss_notices: std::sync::Mutex::new(false),
        history: std::sync::Mutex::new(Vec::new()),
        history_index: std::sync::Mutex::new(-1),
        history_draft: std::sync::Mutex::new(None),
        cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
        events_lagged: std::sync::atomic::AtomicBool::new(false),
        scoped_edit: std::sync::Mutex::new(None),
        markdown_transformer: Arc::new(std::sync::Mutex::new(None)),
        extension_claimed_keys: Mutex::new(Default::default()),
        extension_session: Arc::new(std::sync::Mutex::new(
            rpi_extensions::ExtensionSession::none(),
        )),
        search: Arc::new(AltScreenSearch::new()),
        search_bar: Arc::new(SearchBar::new()),
        pending_container: Arc::new(Container::new()),
        dequeue_hint: String::new(),
        pending_snapshot: std::sync::Mutex::new(
            rpi_harness::agent_harness::QueuedMessages::default(),
        ),
    });
    let editor_container = Arc::new(Container::new());
    let editor = Arc::new(Editor::simple());
    editor_container.add_child(editor.clone());
    assert!(!state.selector_open());

    let tui_terminal = Box::new(ProcessTerminal::new());
    let tui = Arc::new(TuiAltScreen::new(tui_terminal, true, None));
    let list = Arc::new(SelectList::new(
        vec![SelectItem::new("a", "A"), SelectItem::new("b", "B")],
        5,
    ));
    open_selector(
        &state,
        &editor_container,
        &editor,
        &tui,
        list,
        SelectorKind::Theme,
    );
    assert!(state.selector_open());
    // list only (editor swapped out).
    assert_eq!(editor_container.child_count(), 1);

    close_selector(&state, &editor_container, &editor, &tui);
    assert!(!state.selector_open());
    // editor restored.
    assert_eq!(editor_container.child_count(), 1);
}

#[test]
fn test_message_history_browse_restores_draft() {
    // ↑/↓ recall semantics (mirrors TS navigateHistory): push two
    // messages, browse older → newer → back past the newest restores the
    // draft the user was typing.
    let state = Arc::new(TuiState {
        history_page: history::HistoryUi::default(),
        session_dir: std::path::PathBuf::from("unused-session-root"),
        current_assistant: Arc::new(std::sync::Mutex::new(None)),
        tool_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        bash_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        hide_thinking: Arc::new(std::sync::Mutex::new(false)),
        tool_outputs_expanded: Arc::new(std::sync::Mutex::new(false)),
        show_terminal_progress: true,
        status: std::sync::Mutex::new(RunStatus::Idle),
        ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
        ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
        editor_text: rpi_extensions::EditorTextMailbox::new(),
        auto_send: std::sync::Mutex::new(None),
        last_editor_text: std::sync::Mutex::new(String::new()),
        programmatic_editor_write: std::sync::atomic::AtomicBool::new(false),
        js_preparation_cancel: std::sync::Mutex::new(None),
        user_bash_cancel: std::sync::Mutex::new(None),
        pending_bash_messages: std::sync::Mutex::new(Vec::new()),
        footer: Arc::new(FooterComponent::new()),
        status_container: Arc::new(Container::new()),
        chat_container: Arc::new(Container::new()),
        loader: Arc::new(Loader::new()),
        editor: Arc::new(Editor::simple()),
        last_assistant_text: std::sync::Mutex::new(String::new()),
        active_selector: std::sync::Mutex::new(None),
        provider_form: std::sync::Mutex::new(None),
        active_provider_input: std::sync::Mutex::new(None),
        provider_auth_flow_id: std::sync::atomic::AtomicU64::new(0),
        provider_auth_generation: std::sync::atomic::AtomicU64::new(0),
        provider_auth_cancel: std::sync::Mutex::new(None),
        active_provider_select: std::sync::Mutex::new(None),
        active_extension_editor: std::sync::Mutex::new(None),
        active_extension_input: std::sync::Mutex::new(None),
        active_extension_cancel: std::sync::Mutex::new(None),
        autocomplete: AutocompleteManager::new(),
        autocomplete_container: Arc::new(Container::new()),
        autocomplete_max_visible: 5,
        autocomplete_selection: std::sync::Mutex::new(0),
        autocomplete_selection_stamped_for: std::sync::Mutex::new(None),
        pending_images: std::sync::Mutex::new(Vec::new()),
        theme_manager: Arc::new(ThemeManager::new()),
        active_theme_spec: std::sync::Mutex::new(None),
        theme_watcher: std::sync::Mutex::new(None),
        theme_registry: std::sync::RwLock::new(crate::themes::ThemeRegistry::default()),
        tui: None,
        current_model_id: std::sync::Mutex::new(String::new()),
        show_images: std::sync::Mutex::new(true),
        cache_miss_notices: std::sync::Mutex::new(false),
        history: std::sync::Mutex::new(Vec::new()),
        history_index: std::sync::Mutex::new(-1),
        history_draft: std::sync::Mutex::new(None),
        cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
        events_lagged: std::sync::atomic::AtomicBool::new(false),
        scoped_edit: std::sync::Mutex::new(None),
        markdown_transformer: Arc::new(std::sync::Mutex::new(None)),
        extension_claimed_keys: Mutex::new(Default::default()),
        extension_session: Arc::new(std::sync::Mutex::new(
            rpi_extensions::ExtensionSession::none(),
        )),
        search: Arc::new(AltScreenSearch::new()),
        search_bar: Arc::new(SearchBar::new()),
        pending_container: Arc::new(Container::new()),
        dequeue_hint: String::new(),
        pending_snapshot: std::sync::Mutex::new(
            rpi_harness::agent_harness::QueuedMessages::default(),
        ),
    });
    let editor = Arc::new(Editor::simple());

    push_history(&state, "first message");
    push_history(&state, "second message");
    // Consecutive duplicate is skipped.
    push_history(&state, "second message");
    push_history(&state, "   "); // empty → skipped
    assert_eq!(state.history.lock().unwrap().len(), 2);
    assert_eq!(state.history.lock().unwrap()[0], "second message");

    // User starts typing a fresh prompt.
    editor.set_text("half-typed");
    editor.set_cursor(0, 11);

    // ↑ → most recent.
    navigate_history(&state, &editor, -1);
    assert_eq!(editor.get_text(), "second message");
    assert_eq!(*state.history_index.lock().unwrap(), 0);
    // ↑ → older.
    navigate_history(&state, &editor, -1);
    assert_eq!(editor.get_text(), "first message");
    assert_eq!(*state.history_index.lock().unwrap(), 1);
    // ↑ past the oldest → stays (no wrap).
    navigate_history(&state, &editor, -1);
    assert_eq!(editor.get_text(), "first message");
    // ↓ → newer.
    navigate_history(&state, &editor, 1);
    assert_eq!(editor.get_text(), "second message");
    // ↓ past the newest → restores the draft.
    navigate_history(&state, &editor, 1);
    assert_eq!(editor.get_text(), "half-typed");
    assert_eq!(*state.history_index.lock().unwrap(), -1);
}

#[test]
fn test_accept_top_suggestion_replaces_prefix() {
    // `/he` + Tab → `/help ` (slash command provider inserts a space).
    let state = Arc::new(TuiState {
        history_page: history::HistoryUi::default(),
        session_dir: std::path::PathBuf::from("unused-session-root"),
        current_assistant: Arc::new(std::sync::Mutex::new(None)),
        tool_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        bash_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        hide_thinking: Arc::new(std::sync::Mutex::new(false)),
        tool_outputs_expanded: Arc::new(std::sync::Mutex::new(false)),
        show_terminal_progress: true,
        status: std::sync::Mutex::new(RunStatus::Idle),
        ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
        ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
        editor_text: rpi_extensions::EditorTextMailbox::new(),
        auto_send: std::sync::Mutex::new(None),
        last_editor_text: std::sync::Mutex::new(String::new()),
        programmatic_editor_write: std::sync::atomic::AtomicBool::new(false),
        js_preparation_cancel: std::sync::Mutex::new(None),
        user_bash_cancel: std::sync::Mutex::new(None),
        pending_bash_messages: std::sync::Mutex::new(Vec::new()),
        footer: Arc::new(FooterComponent::new()),
        status_container: Arc::new(Container::new()),
        chat_container: Arc::new(Container::new()),
        loader: Arc::new(Loader::new()),
        editor: Arc::new(Editor::simple()),
        last_assistant_text: std::sync::Mutex::new(String::new()),
        active_selector: std::sync::Mutex::new(None),
        provider_form: std::sync::Mutex::new(None),
        active_provider_input: std::sync::Mutex::new(None),
        provider_auth_flow_id: std::sync::atomic::AtomicU64::new(0),
        provider_auth_generation: std::sync::atomic::AtomicU64::new(0),
        provider_auth_cancel: std::sync::Mutex::new(None),
        active_provider_select: std::sync::Mutex::new(None),
        active_extension_editor: std::sync::Mutex::new(None),
        active_extension_input: std::sync::Mutex::new(None),
        active_extension_cancel: std::sync::Mutex::new(None),
        autocomplete: AutocompleteManager::new(),
        autocomplete_container: Arc::new(Container::new()),
        autocomplete_max_visible: 5,
        autocomplete_selection: std::sync::Mutex::new(0),
        autocomplete_selection_stamped_for: std::sync::Mutex::new(None),
        pending_images: std::sync::Mutex::new(Vec::new()),
        theme_manager: Arc::new(ThemeManager::new()),
        active_theme_spec: std::sync::Mutex::new(None),
        theme_watcher: std::sync::Mutex::new(None),
        theme_registry: std::sync::RwLock::new(crate::themes::ThemeRegistry::default()),
        tui: None,
        current_model_id: std::sync::Mutex::new(String::new()),
        show_images: std::sync::Mutex::new(true),
        cache_miss_notices: std::sync::Mutex::new(false),
        history: std::sync::Mutex::new(Vec::new()),
        history_index: std::sync::Mutex::new(-1),
        history_draft: std::sync::Mutex::new(None),
        cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
        events_lagged: std::sync::atomic::AtomicBool::new(false),
        scoped_edit: std::sync::Mutex::new(None),
        markdown_transformer: Arc::new(std::sync::Mutex::new(None)),
        extension_claimed_keys: Mutex::new(Default::default()),
        extension_session: Arc::new(std::sync::Mutex::new(
            rpi_extensions::ExtensionSession::none(),
        )),
        search: Arc::new(AltScreenSearch::new()),
        search_bar: Arc::new(SearchBar::new()),
        pending_container: Arc::new(Container::new()),
        dequeue_hint: String::new(),
        pending_snapshot: std::sync::Mutex::new(
            rpi_harness::agent_harness::QueuedMessages::default(),
        ),
    });
    {
        let mut combined = CombinedAutocompleteProvider::new();
        combined.add_provider(Arc::new(SlashCommandAutocompleteProvider::new(
            build_builtin_registry().visible_entries(),
        )));
        state.autocomplete.set_provider(Arc::new(combined));
    }
    let editor = Arc::new(Editor::simple());
    editor.set_text("/he");
    editor.set_cursor(0, 3);
    refresh_autocomplete(&state, &editor);
    let accepted = accept_top_suggestion(&state, &editor);
    assert!(accepted, "should accept the top suggestion");
    let text = editor.get_text();
    assert!(
        text.starts_with("/help"),
        "editor text should start with /help, got {text}"
    );
}

/// Build a `TuiState` for autocomplete tests (autocomplete providers are
/// installed by each test).
fn autocomplete_test_state(max_visible: usize) -> Arc<TuiState> {
    Arc::new(TuiState {
        history_page: history::HistoryUi::default(),
        session_dir: std::path::PathBuf::from("unused-session-root"),
        current_assistant: Arc::new(std::sync::Mutex::new(None)),
        tool_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        bash_components: Arc::new(std::sync::Mutex::new(HashMap::new())),
        hide_thinking: Arc::new(std::sync::Mutex::new(false)),
        tool_outputs_expanded: Arc::new(std::sync::Mutex::new(false)),
        show_terminal_progress: true,
        status: std::sync::Mutex::new(RunStatus::Idle),
        ext_status: rpi_extensions::ExtensionStatusMailbox::new(),
        ext_status_revision: std::sync::atomic::AtomicU64::new(u64::MAX),
        editor_text: rpi_extensions::EditorTextMailbox::new(),
        auto_send: std::sync::Mutex::new(None),
        last_editor_text: std::sync::Mutex::new(String::new()),
        programmatic_editor_write: std::sync::atomic::AtomicBool::new(false),
        js_preparation_cancel: std::sync::Mutex::new(None),
        user_bash_cancel: std::sync::Mutex::new(None),
        pending_bash_messages: std::sync::Mutex::new(Vec::new()),
        footer: Arc::new(FooterComponent::new()),
        status_container: Arc::new(Container::new()),
        chat_container: Arc::new(Container::new()),
        loader: Arc::new(Loader::new()),
        editor: Arc::new(Editor::simple()),
        last_assistant_text: std::sync::Mutex::new(String::new()),
        active_selector: std::sync::Mutex::new(None),
        provider_form: std::sync::Mutex::new(None),
        active_provider_input: std::sync::Mutex::new(None),
        provider_auth_flow_id: std::sync::atomic::AtomicU64::new(0),
        provider_auth_generation: std::sync::atomic::AtomicU64::new(0),
        provider_auth_cancel: std::sync::Mutex::new(None),
        active_provider_select: std::sync::Mutex::new(None),
        active_extension_editor: std::sync::Mutex::new(None),
        active_extension_input: std::sync::Mutex::new(None),
        active_extension_cancel: std::sync::Mutex::new(None),
        autocomplete: AutocompleteManager::new(),
        autocomplete_container: Arc::new(Container::new()),
        autocomplete_max_visible: max_visible,
        autocomplete_selection: std::sync::Mutex::new(0),
        autocomplete_selection_stamped_for: std::sync::Mutex::new(None),
        pending_images: std::sync::Mutex::new(Vec::new()),
        theme_manager: Arc::new(ThemeManager::new()),
        active_theme_spec: std::sync::Mutex::new(None),
        theme_watcher: std::sync::Mutex::new(None),
        theme_registry: std::sync::RwLock::new(crate::themes::ThemeRegistry::default()),
        tui: None,
        current_model_id: std::sync::Mutex::new(String::new()),
        show_images: std::sync::Mutex::new(true),
        cache_miss_notices: std::sync::Mutex::new(false),
        history: std::sync::Mutex::new(Vec::new()),
        history_index: std::sync::Mutex::new(-1),
        history_draft: std::sync::Mutex::new(None),
        cache_tracker: std::sync::Mutex::new(rpi_harness::cache_stats::CacheMissTracker::new()),
        events_lagged: std::sync::atomic::AtomicBool::new(false),
        scoped_edit: std::sync::Mutex::new(None),
        markdown_transformer: Arc::new(std::sync::Mutex::new(None)),
        extension_claimed_keys: Mutex::new(Default::default()),
        extension_session: Arc::new(std::sync::Mutex::new(
            rpi_extensions::ExtensionSession::none(),
        )),
        search: Arc::new(AltScreenSearch::new()),
        search_bar: Arc::new(SearchBar::new()),
        pending_container: Arc::new(Container::new()),
        dequeue_hint: String::new(),
        pending_snapshot: std::sync::Mutex::new(
            rpi_harness::agent_harness::QueuedMessages::default(),
        ),
    })
}

#[test]
fn autocomplete_navigation_reaches_beyond_visible_window() {
    let state = autocomplete_test_state(5);
    let commands: Vec<rpi_tui::SlashCommand> = (0..7)
        .map(|i| rpi_tui::SlashCommand {
            name: format!("/choice{i}"),
            description: format!("choice {i}"),
        })
        .collect();
    state
        .autocomplete
        .set_provider(Arc::new(SlashCommandAutocompleteProvider::new(commands)));
    let editor = Arc::new(Editor::simple());
    editor.set_text("/");
    editor.set_cursor(0, 1);
    refresh_autocomplete(&state, &editor);

    // 5 × ↓ must move the highlight to index 5 (the 6th item), past the
    // 5-row visible window (previously it clamped at max_visible - 1 = 4).
    for _ in 0..5 {
        assert!(navigate_autocomplete(&state, &editor, 1));
    }
    assert_eq!(*state.autocomplete_selection.lock().unwrap(), 5);
    assert!(accept_top_suggestion(&state, &editor));
    assert!(
        editor.get_text().starts_with("/choice5"),
        "Tab should accept the highlighted 6th item, got {}",
        editor.get_text()
    );
}

#[test]
fn autocomplete_navigation_does_not_swallow_plain_draft_arrows() {
    let state = autocomplete_test_state(5);
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("docs")).unwrap();
    let mut combined = CombinedAutocompleteProvider::new();
    combined.add_provider(Arc::new(FilePathAutocompleteProvider::with_root(
        tmp.path().to_path_buf(),
    )));
    state.autocomplete.set_provider(Arc::new(combined));

    let editor = Arc::new(Editor::simple());
    editor.set_text("first line\nplease check docs");
    editor.set_cursor(1, 18);
    // A bare-word path completion (`docs`) must not consume ↑ — the editor
    // owns cursor movement outside explicit `/` or `@` completion.
    assert!(!navigate_autocomplete(&state, &editor, -1));
}

#[test]
fn autocomplete_navigation_handles_leading_at_file_completion() {
    let state = autocomplete_test_state(5);
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("docs_a")).unwrap();
    std::fs::create_dir_all(tmp.path().join("docs_b")).unwrap();
    state
        .autocomplete
        .set_provider(Arc::new(FilePathAutocompleteProvider::with_root(
            tmp.path().to_path_buf(),
        )));
    let editor = Arc::new(Editor::simple());
    editor.set_text("@do");
    editor.set_cursor(0, 3);
    refresh_autocomplete(&state, &editor);
    // The path provider keeps `@` and puts `start` after it; ↓ must still work.
    assert!(navigate_autocomplete(&state, &editor, 1));
    assert_eq!(*state.autocomplete_selection.lock().unwrap(), 1);
    assert!(accept_top_suggestion(&state, &editor));
    assert!(
        editor.get_text().starts_with("@docs_"),
        "@ plus the picked candidate must be inserted, got {}",
        editor.get_text()
    );
}

#[test]
fn autocomplete_navigation_handles_at_file_after_chinese_draft() {
    let state = autocomplete_test_state(5);
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("docs_a")).unwrap();
    std::fs::create_dir_all(tmp.path().join("docs_b")).unwrap();
    state
        .autocomplete
        .set_provider(Arc::new(FilePathAutocompleteProvider::with_root(
            tmp.path().to_path_buf(),
        )));
    let editor = Arc::new(Editor::simple());
    editor.set_text("请查看 @do");
    editor.set_cursor(0, 13);
    refresh_autocomplete(&state, &editor);
    assert!(navigate_autocomplete(&state, &editor, 1));
    assert!(accept_top_suggestion(&state, &editor));
    assert!(
        editor.get_text().starts_with("请查看 @docs_"),
        "draft, @, and the picked candidate must be preserved, got {}",
        editor.get_text()
    );
}

#[test]
fn configured_key_parser_supports_native_notation() {
    let combo = parse_configured_key("Ctrl+G").expect("ctrl+g should parse");
    assert_eq!(combo.code, KeyCode::Char('g'));
    assert!(combo.modifiers.contains(KeyModifiers::CONTROL));
    let combo = parse_configured_key("shift+tab").expect("shift+tab should parse");
    assert_eq!(combo.code, KeyCode::BackTab);
}

#[test]
fn double_escape_trigger_has_half_second_window() {
    let now = std::time::Instant::now();
    assert!(!double_escape_trigger(None, now));
    assert!(double_escape_trigger(
        Some(now - std::time::Duration::from_millis(500)),
        now
    ));
    assert!(!double_escape_trigger(
        Some(now - std::time::Duration::from_millis(501)),
        now
    ));
}

#[test]
fn ctrl_c_clears_first_and_only_exits_inside_the_window() {
    // upstream's `handleCtrlC`: a lone press clears the editor and never
    // quits by itself; only a second press within 500ms exits. It must not
    // abort — Esc owns that.
    let now = std::time::Instant::now();
    assert_eq!(ctrl_c_action(None, now), CtrlCAction::Clear);
    assert_eq!(
        ctrl_c_action(Some(now - std::time::Duration::from_millis(500)), now),
        CtrlCAction::Exit
    );
    // A slower second press is a fresh "clear", not a quit.
    assert_eq!(
        ctrl_c_action(Some(now - std::time::Duration::from_millis(501)), now),
        CtrlCAction::Clear
    );
}

#[test]
fn pasted_multi_line_block_becomes_one_prompt_not_many() {
    // End-to-end shape of the fix: replay the key stream a Windows paste
    // produces ("l1\nl2\nl3" + a trailing Enter) through the same pieces
    // the key loop uses — the burst discriminator for each Enter and the
    // editor for the text — and assert that *no* submit fired while the
    // block arrived, then exactly one submit on the user's own Enter.
    use std::sync::atomic::{AtomicUsize, Ordering};

    let editor = rpi_tui::Editor::simple();
    let submits = Arc::new(AtomicUsize::new(0));
    let submits_for_cb = submits.clone();
    editor.on_submit(Arc::new(move |_text: &str| {
        submits_for_cb.fetch_add(1, Ordering::SeqCst);
    }));

    // Every pasted key is delivered back-to-back, so `now` is unchanged and
    // the console queue is never empty until the paste is drained.
    let now = std::time::Instant::now();
    let mut last_text_key_at: Option<std::time::Instant> = None;
    let block = ["l1", "l2", "l3"];
    for (index, line) in block.iter().enumerate() {
        for ch in line.chars() {
            editor.insert(&ch.to_string());
            last_text_key_at = Some(now);
        }
        if index + 1 < block.len() {
            // The pasted newline: more input is still queued behind it.
            let more_queued = true;
            assert!(
                enter_is_paste_burst(last_text_key_at, now, more_queued),
                "pasted newline {index} must not submit"
            );
            editor.insert("\n");
            last_text_key_at = Some(now);
        }
    }
    assert_eq!(editor.get_text(), "l1\nl2\nl3");
    assert_eq!(submits.load(Ordering::SeqCst), 0, "paste must not submit");

    // The user's own Enter: a real gap, nothing queued.
    let typed_at = now + PASTE_BURST_GAP + std::time::Duration::from_millis(80);
    assert!(!enter_is_paste_burst(last_text_key_at, typed_at, false));
    editor.handle_key(crossterm::event::KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    ));
    assert_eq!(
        submits.load(Ordering::SeqCst),
        1,
        "typed Enter submits once"
    );
}

#[test]
fn pasted_win_crlf_text_has_no_stray_carriage_returns() {
    // A Windows paste delivers CRLF; the editor must fold it to LF so the
    // rendered block is clean and the cursor arithmetic stays sound.
    let editor = rpi_tui::Editor::simple();
    editor.insert("alpha\r\nbeta\r\n");
    assert_eq!(editor.get_text(), "alpha\nbeta\n");
    assert!(!editor.get_text().contains('\r'));
}

/// An IME commit delivers a whole word with no gap between its characters, so a
/// gap-based paste heuristic classified normal Chinese input as paste and held
/// it in a buffer until the user stopped typing. The run must instead be handed
/// to the editor on the very keystroke that arrives with nothing queued behind
/// it — which is what this asserts about the state machine itself.
#[test]
fn ime_commit_is_not_buffered_as_paste() {
    let mut run = WindowsPasteRun::default();
    let now = std::time::Instant::now();

    // `我们` arrives back-to-back: burst = true, but far too short to fold.
    run.feed('我', now);
    run.feed('们', now);
    assert!(run.burst, "back-to-back characters are a burst");
    assert!(
        !run.is_paste_candidate(),
        "a two-character IME commit must never be treated as a paste"
    );

    // Flushing it empties the buffer, so the next word starts clean.
    let editor = rpi_tui::Editor::simple();
    run.flush(&editor, now);
    assert_eq!(editor.get_text(), "我们");
    assert!(!run.is_buffering(), "flush must clear the buffer");

    // A genuine clipboard paste clears the size threshold.
    let mut big = WindowsPasteRun::default();
    for _ in 0..PASTE_RUN_MIN_CHARS {
        big.feed('x', now);
    }
    assert!(big.is_paste_candidate(), "a long run is still foldable");
}

/// The console can hand one clipboard payload over in several batches. When
/// the batch holding the final newline arrives after the text batch was
/// already flushed to the editor, the run is empty — but that newline still
/// belongs to the paste and must not submit the prompt.
///
/// The flushed batch here carries an interior newline: a multi-line payload
/// whose first console batch ends after its second line. That shape is what
/// separates it from a coalesced IME commit, which is also queued behind
/// itself but never contains a newline — see
/// [`final_review_pr91_coalesced_ime_commit_never_leaves_a_tail_that_eats_enter`].
#[test]
fn paste_split_just_before_its_trailing_enter_is_not_a_submit() {
    let now = std::time::Instant::now();
    let clipboard = "alpha\nbeta\nc\nd\ne\nf\ng\nh\ni\nj\nk\n";
    let editor = rpi_tui::Editor::simple();

    let mut run = WindowsPasteRun::default();
    for ch in "alpha\nbeta".chars() {
        run.feed(ch, now);
    }
    // The console proved this is a paste: input was still queued behind it.
    run.mark_collected();
    run.flush(&editor, now);
    assert_eq!(editor.get_text(), "alpha\nbeta");
    assert!(!run.is_buffering(), "the text batch is already inserted");

    // The newline lands in the next batch, inside the carry window, and the
    // clipboard confirms which text it belongs to.
    let entered_at = now + std::time::Duration::from_millis(60);
    assert!(
        windows_enter_in_paste(&run, false, Some(clipboard), entered_at),
        "a newline split from its paste must not submit"
    );

    // A genuinely later Enter is the user's own and has to submit.
    let typed_at = now + PASTE_CARRY_GAP + std::time::Duration::from_millis(1);
    assert!(
        !windows_enter_in_paste(&run, false, Some(clipboard), typed_at),
        "a typed Enter after the carry window still submits"
    );
}

/// The behaviour this whole change exists for: an IME commit must never absorb
/// the Enter that submits it. Crucially this has to hold even when the clipboard
/// happens to start with the committed word — otherwise the guard only works by
/// accident, because the test clipboard did not match.
#[test]
fn ime_commit_then_enter_submits_even_when_the_clipboard_starts_with_it() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();

    // Deliberately corroborating: same head word, and enough newlines for the
    // clipboard check to pass if it were ever consulted.
    let clipboard = "我们\n第二行\n3\n4\n5\n6\n7\n8\n9\n10\n11\n";

    let mut ime = WindowsPasteRun::default();
    ime.feed('我', now);
    ime.feed('们', now);
    // The typing path: the queue was empty, so nothing was collected.
    ime.flush(&editor, now);

    assert_eq!(editor.get_text(), "我们");
    assert!(
        !ime.has_paste_evidence(now),
        "an IME commit must leave no paste evidence behind"
    );
    assert!(
        ime.paste_tail_texts(now).is_empty(),
        "an IME commit must not keep a paste tail"
    );

    let entered_at = now + std::time::Duration::from_millis(80);
    assert!(
        !windows_enter_in_paste(&ime, false, Some(clipboard), entered_at),
        "an IME commit must never absorb the Enter that submits it, even when \
         the clipboard starts with the committed word"
    );
}

/// Two pastes inside the carry window are two separate pastes. Gluing the second
/// batch onto the first makes the corroborated prefix match neither clipboard,
/// which is exactly how a trailing Enter slips through and submits the prompt.
#[test]
fn consecutive_pastes_each_keep_their_own_trailing_enter() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let clipboard_a = "AAAA\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    let clipboard_b = "BBBB\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";

    let mut run = WindowsPasteRun::default();
    for ch in "AAAA".chars() {
        run.feed(ch, now);
    }
    run.mark_collected();
    run.flush(&editor, now);

    // Second paste starts inside `PASTE_CARRY_GAP`.
    let second_at = now + std::time::Duration::from_millis(120);
    for ch in "BBBB".chars() {
        run.feed(ch, second_at);
    }
    run.mark_collected();
    run.flush(&editor, second_at);
    assert_eq!(editor.get_text(), "AAAABBBB");

    // The candidates must include the second paste alone, so its own trailing
    // Enter corroborates — even though the first paste is still carried too.
    assert!(
        run.paste_tail_texts(second_at).iter().any(|t| t == "BBBB"),
        "the tail has to include the head of the most recent paste alone"
    );

    // The trailing Enter trails its batch at machine speed — a console-level
    // split of the payload, not a human key-to-key delay (two IME commits in
    // the same shape must submit instead: the standalone lone-short window
    // is `PASTE_BURST_GAP`).
    let enter_at = second_at + std::time::Duration::from_millis(10);
    assert!(
        windows_enter_in_paste(&run, false, Some(clipboard_b), enter_at),
        "the second paste's trailing Enter must not submit"
    );
    // And it must not corroborate against the *first* clipboard either.
    assert!(
        !windows_enter_in_paste(&run, false, Some(clipboard_a), enter_at),
        "a stale clipboard must not be accepted as evidence"
    );
}

/// A paste that ends without a trailing newline must still leave its text
/// available: the console can split the payload anywhere, not only before the
/// final newline.
#[test]
fn paste_tail_is_dropped_once_the_carry_window_expires() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let clipboard = "alpha\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n";

    let mut run = WindowsPasteRun::default();
    for ch in "alpha".chars() {
        run.feed(ch, now);
    }
    run.mark_collected();
    run.flush(&editor, now);

    assert!(run.has_paste_evidence(now));
    let expired = now + PASTE_CARRY_GAP + std::time::Duration::from_millis(1);
    assert!(!run.has_paste_evidence(expired));
    assert!(run.paste_tail_texts(expired).is_empty());
    assert!(!windows_enter_in_paste(
        &run,
        false,
        Some(clipboard),
        expired
    ));
}

/// Once a run crosses the fold threshold the loop must stop probing per
/// character: the probe stashes the next event, and that stash is what made the
/// native console drain unreachable.
#[test]
fn native_batch_read_is_reached_before_the_queue_probe() {
    let now = std::time::Instant::now();

    let mut run = WindowsPasteRun::default();
    for _ in 0..PASTE_RUN_MIN_CHARS {
        run.feed('x', now);
    }
    assert!(run.is_paste_candidate());
    assert!(
        !run.probe_needed(),
        "an established paste must drain the console rather than stash a probe"
    );
    assert!(
        should_drain_queued_text(&run, true),
        "the native batch read has to be reachable"
    );
    assert!(
        !should_drain_queued_text(&run, false),
        "claimed extension keys keep ownership of the console"
    );

    // A short IME commit still probes, which is what keeps typing immediate.
    let mut ime = WindowsPasteRun::default();
    ime.feed('我', now);
    ime.feed('们', now);
    assert!(ime.probe_needed());
}

#[test]
fn pasted_enter_is_not_a_submit() {
    let now = std::time::Instant::now();

    // Queued input behind the Enter => paste, regardless of history.
    assert!(enter_is_paste_burst(None, now, true));

    // Enter immediately after pasted characters => paste (also covers a
    // paste whose final line ends with a newline, where nothing is queued).
    assert!(enter_is_paste_burst(
        Some(now - std::time::Duration::from_millis(1)),
        now,
        false
    ));
    assert!(enter_is_paste_burst(
        Some(now - (PASTE_BURST_GAP - std::time::Duration::from_millis(1))),
        now,
        false
    ));

    // A typed Enter: nothing queued and a real gap since the last key.
    assert!(!enter_is_paste_burst(None, now, false));
    assert!(!enter_is_paste_burst(
        Some(now - PASTE_BURST_GAP),
        now,
        false
    ));
    assert!(!enter_is_paste_burst(
        Some(now - std::time::Duration::from_millis(120)),
        now,
        false
    ));
}

#[test]
fn paste_probe_ignores_the_enter_keys_own_release() {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    let release =
        KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release);

    // Windows queues press+release together over RDP: the release behind a
    // lone Enter must NOT count as queued paste content.
    let mut queued = vec![Event::Key(release)].into_iter();
    let (more, stashed) = probe_paste_input(None, || queued.next());
    assert!(!more, "a key release alone is not queued paste input");
    assert!(stashed.is_none());

    // A real follow-up key (the next pasted line) IS queued input and is
    // stashed for the caller instead of being lost.
    let next = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
    let mut queued = vec![Event::Key(release), Event::Key(next)].into_iter();
    let (more, stashed) = probe_paste_input(None, || queued.next());
    assert!(more, "a queued key press is paste continuation");
    assert!(matches!(stashed, Some(Event::Key(_))));

    // Nothing queued at all.
    let (more, stashed) = probe_paste_input(None, || None);
    assert!(!more);
    assert!(stashed.is_none());

    // An already-stashed event counts as queued input, without reading more.
    let (more, stashed) = probe_paste_input(Some(Event::Key(next)), || {
        panic!("must not read when an event is already stashed")
    });
    assert!(more);
    assert!(matches!(stashed, Some(Event::Key(_))));
}

// ---- ask_user TUI bridge ----

#[test]
fn ask_user_freeform_submit_releases_dialog_locks_before_callback() {
    let state = test_tui_state();
    let mailbox = rpi_extensions::UiDialogMailbox::new();
    let bridge = AskUserBridge::new(mailbox.clone());
    bridge.attach();
    for id in ["freeform", "next"] {
        mailbox
            .handle(serde_json::json!({
                "op": "open", "requestId": id,
                "ui": {"kind": "input", "question": "Answer?"}
            }))
            .unwrap();
    }
    let request = bridge.take_pending_for_tui().unwrap();
    let input = Arc::new(Input::new());
    input.handle_paste("自定义回答");
    let state_submit = state.clone();
    let bridge_submit = bridge.clone();
    input.on_submit(Arc::new(move |text| {
        // Fail immediately instead of hanging the test on a recursive lock.
        let mut editor = state_submit
            .active_extension_editor
            .try_lock()
            .expect("editor slot locked during submit");
        let mut input = state_submit
            .active_extension_input
            .try_lock()
            .expect("input slot locked during submit");
        bridge_submit.respond(&request.request_id, serde_json::json!({"text": text}));
        *editor = None;
        *input = None;
    }));
    *state.active_extension_input.lock().unwrap() = Some(input);
    handle_extension_dialog_key(&state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!state.extension_dialog_open());
    assert_eq!(
        mailbox.poll("freeform").unwrap()["answer"]["text"],
        "自定义回答"
    );
    assert_eq!(bridge.take_pending_for_tui().unwrap().request_id, "next");
}

#[test]
fn extension_editor_submit_releases_dialog_locks_before_callback() {
    let state = test_tui_state();
    let editor = Arc::new(Editor::simple());
    editor.set_text("response");
    let state_submit = state.clone();
    editor.on_submit(Arc::new(move |text| {
        assert_eq!(text, "response");
        *state_submit
            .active_extension_editor
            .try_lock()
            .expect("editor slot locked during submit") = None;
        *state_submit
            .active_extension_input
            .try_lock()
            .expect("input slot locked during submit") = None;
    }));
    *state.active_extension_editor.lock().unwrap() = Some(editor);
    handle_extension_dialog_key(&state, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!state.extension_dialog_open());
}

#[test]
fn ask_user_prompt_parser_supports_flat_and_questions_shapes() {
    let flat = rpi_extensions::UiDialogRequest {
        request_id: "r1".into(),
        tool_call_id: Some("c1".into()),
        ui: serde_json::json!({
            "kind": "selector",
            "id": "proxy_port",
            "header": "Proxy",
            "question": "端口?",
            "options": [{"title": "7890", "description": "text"}, {"title": "7897"}],
            "allowMultiple": true,
            "allowFreeform": false,
            "suggest": "1080"
        }),
        raw: serde_json::json!({}),
    };
    let prompt = parse_ask_user_prompt(&flat);
    assert_eq!(prompt.question_id, "proxy_port");
    assert_eq!(prompt.header.as_deref(), Some("Proxy"));
    assert_eq!(prompt.options.len(), 2);
    assert_eq!(prompt.options[0].1.as_deref(), Some("text"));
    assert!(prompt.allow_multiple);
    assert!(!prompt.allow_freeform);
    assert_eq!(prompt.suggest.as_deref(), Some("1080"));

    let alias = rpi_extensions::UiDialogRequest {
        ui: serde_json::json!({
            "questions": [{"id": "q", "question": "Q?", "options": []}]
        }),
        ..flat.clone()
    };
    let prompt = parse_ask_user_prompt(&alias);
    assert_eq!(prompt.question_id, "q");
    assert!(prompt.allow_freeform, "no options defaults to freeform");

    let confirm = rpi_extensions::UiDialogRequest {
        ui: serde_json::json!({"kind": "confirm", "question": "Go?"}),
        ..flat.clone()
    };
    let prompt = parse_ask_user_prompt(&confirm);
    assert_eq!(prompt.kind, "confirm");
    assert_eq!(prompt.options.len(), 2);
    assert_eq!(prompt.options[0].0, "Yes");
}

#[test]
fn ask_user_bridge_answers_and_cancels_by_request_id() {
    let mailbox = rpi_extensions::UiDialogMailbox::new();
    mailbox.attach();
    let bridge = AskUserBridge::new(mailbox.clone());

    // Two concurrent requests must not cross answers.
    mailbox
        .handle(serde_json::json!({
            "op": "open", "requestId": "r1", "toolCallId": "call-r1",
            "ui": {"kind": "input", "question": "Port?"}
        }))
        .unwrap();
    mailbox
        .handle(serde_json::json!({
            "op": "open", "requestId": "r2", "toolCallId": "call-r2",
            "ui": {"kind": "input", "question": "Level?"}
        }))
        .unwrap();

    let first = bridge.take_pending_for_tui().expect("r1 visible");
    assert_eq!(first.request_id, "r1");
    assert!(
        bridge.take_pending_for_tui().is_none(),
        "only one request occupies the input slot"
    );
    bridge.respond("r1", serde_json::json!({"text": "7897"}));
    assert_eq!(mailbox.poll("r1").unwrap()["answer"]["text"], "7897");

    let second = bridge.take_pending_for_tui().expect("r2 visible");
    assert_eq!(second.request_id, "r2");
    bridge.cancel("r2");
    assert_eq!(mailbox.poll("r2").unwrap()["status"], "cancelled");

    // cancel_all drops anything still queued.
    mailbox
        .handle(serde_json::json!({
            "op": "open", "requestId": "r3",
            "ui": {"kind": "input", "question": "X?"}
        }))
        .unwrap();
    bridge.cancel_all();
    assert_eq!(mailbox.poll("r3").unwrap()["status"], "cancelled");

    // A detached mailbox rejects new prompts (headless contract).
    bridge.shutdown();
    assert!(mailbox
        .handle(serde_json::json!({
            "op": "open", "requestId": "r4", "ui": {"kind": "input"}
        }))
        .is_err());
}

// Local review probes for PR #91. Exercise the real state/probe methods using
// the Press/Release queue consumed by the Windows text-key branch.
fn review_pr91_deliver_batch(
    run: &mut WindowsPasteRun,
    editor: &rpi_tui::Editor,
    text: &str,
    trailing_enter: bool,
    now: std::time::Instant,
) -> bool {
    let mut queue = std::collections::VecDeque::new();
    for ch in text.chars() {
        for kind in [KeyEventKind::Press, KeyEventKind::Release] {
            queue.push_back(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char(ch),
                KeyModifiers::NONE,
                kind,
            )));
        }
    }
    if trailing_enter {
        queue.push_back(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
    }
    let mut pending = None;
    while let Some(event) = pending.take().or_else(|| queue.pop_front()) {
        let Event::Key(key) = event else {
            unreachable!()
        };
        if !should_dispatch_key(key.kind) {
            continue;
        }
        if key.code == KeyCode::Enter {
            assert!(queue.is_empty());
            return true;
        }
        let KeyCode::Char(ch) = key.code else {
            unreachable!()
        };
        run.feed(ch, now);
        assert!(
            run.probe_needed(),
            "these short batches do not use the native drain"
        );
        let (more, stashed) = probe_paste_input(pending.take(), || queue.pop_front());
        pending = stashed;
        if more {
            run.mark_collected();
        } else {
            run.flush(editor, now);
        }
    }
    false
}

#[test]
fn review_pr91_one_paste_split_into_three_queue_batches_keeps_its_prefix() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let mut run = WindowsPasteRun::default();
    let clipboard = "alphabeta\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    review_pr91_deliver_batch(&mut run, &editor, "alpha", false, now);
    review_pr91_deliver_batch(
        &mut run,
        &editor,
        "beta",
        false,
        now + std::time::Duration::from_millis(10),
    );
    assert_eq!(editor.get_text(), "alphabeta");
    assert!(
        windows_enter_in_paste(
            &run,
            false,
            Some(clipboard),
            now + std::time::Duration::from_millis(20)
        ),
        "the third batch's Enter must not submit a partially received paste"
    );
}

#[test]
fn review_pr91_second_paste_enter_ignores_the_previous_paste_tail() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let mut run = WindowsPasteRun::default();
    let clipboard = "BBBB\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    review_pr91_deliver_batch(&mut run, &editor, "AAAA", false, now);
    let second_at = now + std::time::Duration::from_millis(20);
    assert!(review_pr91_deliver_batch(
        &mut run, &editor, "BBBB", true, second_at
    ));
    assert_eq!(run.text, "BBBB");
    assert!(
        windows_enter_in_paste(&run, false, Some(clipboard), second_at),
        "a previous paste's tail must not prevent this paste's newline from matching"
    );
}

#[test]
fn review_pr91_queued_ime_commit_can_submit_after_two_hundred_ms() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let mut run = WindowsPasteRun::default();
    let clipboard = "我们\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    review_pr91_deliver_batch(&mut run, &editor, "我们", false, now);
    assert_eq!(editor.get_text(), "我们");
    assert!(
        !windows_enter_in_paste(
            &run,
            false,
            Some(clipboard),
            now + std::time::Duration::from_millis(200)
        ),
        "queued IME characters must not swallow a subsequent typed Enter"
    );
}

#[test]
fn review_pr91_single_character_ime_batches_leave_no_tail() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let mut run = WindowsPasteRun::default();
    review_pr91_deliver_batch(&mut run, &editor, "我", false, now);
    review_pr91_deliver_batch(
        &mut run,
        &editor,
        "们",
        false,
        now + std::time::Duration::from_millis(1),
    );
    assert_eq!(editor.get_text(), "我们");
    assert!(!run.has_paste_evidence(now + std::time::Duration::from_millis(200)));
}

// Re-review probes for PR #91: boundary cases of the candidate construction.
#[test]
fn rereview_pr91_first_paste_with_queued_trailing_enter_does_not_submit() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let mut run = WindowsPasteRun::default();
    let text = "alpha\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10";
    let clipboard = format!("{text}\n");
    assert!(review_pr91_deliver_batch(
        &mut run, &editor, text, true, now
    ));
    assert!(
        run.tail.is_none(),
        "nothing has been flushed before the queued Enter"
    );
    assert_eq!(run.text, text);
    assert!(run.has_paste_evidence(now));
    assert!(
        windows_enter_in_paste(&run, false, Some(&clipboard), now),
        "a live first paste must be corroborated without requiring an older flushed tail"
    );
}

#[test]
fn rereview_pr91_split_paste_includes_both_flushed_and_live_text() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let mut run = WindowsPasteRun::default();
    let clipboard = "alphabeta\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    review_pr91_deliver_batch(&mut run, &editor, "alpha", false, now);
    let later = now + std::time::Duration::from_millis(10);
    assert!(review_pr91_deliver_batch(
        &mut run, &editor, "beta", true, later
    ));
    assert_eq!(run.tail.as_ref().unwrap().text, "alpha");
    assert_eq!(run.text, "beta");
    assert!(
        windows_enter_in_paste(&run, false, Some(clipboard), later),
        "the clipboard prefix must include both alpha (flushed) and beta (still buffered)"
    );
}

#[test]
fn rereview_pr91_completed_paste_must_not_swallow_the_next_edited_submit() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let mut run = WindowsPasteRun::default();
    let text = "alpha\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10";
    let clipboard = format!("{text}\n");
    review_pr91_deliver_batch(&mut run, &editor, text, false, now);
    let pasted_enter = now + std::time::Duration::from_millis(10);
    assert!(windows_enter_in_paste(
        &run,
        false,
        Some(&clipboard),
        pasted_enter
    ));
    run.feed('\n', pasted_enter); // The actual Enter handler accepts the pasted newline.
    let edited_at = now + std::time::Duration::from_millis(80);
    assert!(review_pr91_deliver_batch(
        &mut run, &editor, "X", true, edited_at
    ));
    assert_eq!(run.text, "\nX");
    assert!(
        !windows_enter_in_paste(&run, false, Some(&clipboard), edited_at),
        "a stale matching prefix must not override the unrelated live edit when Enter submits"
    );
}

// Carry-review probes for PR #91: batch boundaries of the first line and
// per-variant maturity of the tail.
#[test]
fn carry_review_pr91_first_line_can_end_at_a_console_batch_boundary() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let mut run = WindowsPasteRun::default();
    let clipboard = "alpha\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    // Only the first line and its Enter have reached the queue. The rest of
    // this same clipboard payload will arrive in a subsequent console batch.
    assert!(review_pr91_deliver_batch(
        &mut run, &editor, "alpha", true, now
    ));
    assert!(run.tail.is_none());
    assert_eq!(run.text, "alpha");
    assert!(run.burst && run.collected);
    assert!(run.has_paste_evidence(now));
    assert!(
        windows_enter_in_paste(&run, false, Some(clipboard), now),
        "the first pasted newline must not submit just because later lines are in another batch"
    );
}

#[test]
fn carry_review_pr91_a_prior_large_paste_must_not_extend_a_new_ime_commits_window() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let mut run = WindowsPasteRun::default();
    // An already completed large paste leaves mature carry. It has no trailing
    // newline, so there is no pending pasted Enter waiting to be attributed.
    for _ in 0..PASTE_RUN_MIN_CHARS {
        run.feed('x', now);
    }
    run.mark_collected();
    run.flush(&editor, now);
    assert!(run.tail.as_ref().unwrap().mature);
    let commit_at = now + std::time::Duration::from_millis(50);
    review_pr91_deliver_batch(&mut run, &editor, "我们", false, commit_at);
    assert_eq!(run.tail.as_ref().unwrap().last, "我们");
    assert!(run.text.is_empty());
    let clipboard = "我们\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    let typed_enter = commit_at + std::time::Duration::from_millis(200);
    assert!(
        !windows_enter_in_paste(&run, false, Some(clipboard), typed_enter),
        "the previous paste must not give a new two-character IME commit a 500ms carry window"
    );
}

// Re-review probe (round 5) for PR #91: a coalesced IME commit delivered as
// one queued batch sets `collected` through the queue probe, so its flush
// leaves a short PasteTail. The clipboard happening to share the commit's
// prefix must not make that tail eligible to swallow the user's Enter.
#[test]
fn final_review_pr91_coalesced_ime_commit_never_leaves_a_tail_that_eats_enter() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let mut run = WindowsPasteRun::default();
    let clipboard = "我们\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    // One queued batch: 我们's Press/Release pairs, exactly as a coalesced
    // IME commit arrives. No newline ever enters this batch.
    review_pr91_deliver_batch(&mut run, &editor, "我们", false, now);
    assert_eq!(editor.get_text(), "我们");
    // Enter arrives within the short carry window (~80 ms).
    let entered_at = now + std::time::Duration::from_millis(80);
    assert!(
        !windows_enter_in_paste(&run, false, Some(clipboard), entered_at),
        "a coalesced IME commit must not leave a tail that can consume a later Enter"
    );
}

// Round-6 review probes for PR #91: the first newline can land in its own
// console batch while the rest of the payload is still in transit, and a
// carried tail must not re-enable a lone short last batch.
#[test]
fn latest_review_pr91_first_newline_can_be_its_own_console_batch() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let mut run = WindowsPasteRun::default();
    let clipboard = "alpha\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    // The same clipboard arrives in order as [alpha], [Enter], [remaining lines].
    // At the Enter, no later event has reached the queue within PASTE_PROBE.
    review_pr91_deliver_batch(&mut run, &editor, "alpha", false, now);
    let entered_at = now + std::time::Duration::from_millis(10);
    assert_eq!(editor.get_text(), "alpha");
    assert!(run.tail.as_ref().unwrap().lone_short);
    assert!(run.has_paste_evidence(entered_at));
    assert!(
        windows_enter_in_paste(&run, false, Some(clipboard), entered_at),
        "a batch boundary before the first newline must not submit the partial clipboard"
    );
}

#[test]
fn latest_review_pr91_old_paste_must_not_enable_a_short_ime_tail() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let mut run = WindowsPasteRun::default();
    for _ in 0..PASTE_RUN_MIN_CHARS {
        run.feed('x', now);
    }
    run.mark_collected();
    run.flush(&editor, now);
    let commit_at = now + std::time::Duration::from_millis(50);
    review_pr91_deliver_batch(&mut run, &editor, "我们", false, commit_at);
    let tail = run.tail.as_ref().unwrap();
    assert!(!tail.lone_short);
    assert!(!tail.last_mature);
    assert_eq!(tail.last, "我们");
    let typed_at = commit_at + std::time::Duration::from_millis(80);
    let clipboard = "我们\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    assert!(
        !windows_enter_in_paste(&run, false, Some(clipboard), typed_at),
        "an older paste must not make a new lone IME batch eligible to swallow Enter"
    );
}

#[test]
fn latest_review_pr91_two_ime_commits_must_not_enable_the_last_tail() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let mut run = WindowsPasteRun::default();
    review_pr91_deliver_batch(&mut run, &editor, "你好", false, now);
    let commit_at = now + std::time::Duration::from_millis(50);
    review_pr91_deliver_batch(&mut run, &editor, "我们", false, commit_at);
    let clipboard = "我们\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    assert!(
        !windows_enter_in_paste(
            &run,
            false,
            Some(clipboard),
            commit_at + std::time::Duration::from_millis(80)
        ),
        "two newline-less IME commits must not enable the standalone last candidate"
    );
}

// Flanking probes around the machine-speed boundary the fix draws for lone
// short tails: corroborate at batch-trailing speed, submit beyond it.
#[test]
fn round6_pr91_lone_short_tail_corroborates_within_the_burst_gap() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let mut run = WindowsPasteRun::default();
    let clipboard = "alpha\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    review_pr91_deliver_batch(&mut run, &editor, "alpha", false, now);
    assert!(
        windows_enter_in_paste(
            &run,
            false,
            Some(clipboard),
            now + std::time::Duration::from_millis(15)
        ),
        "an Enter trailing its batch at machine speed is the paste's own newline"
    );
}

#[test]
fn round6_pr91_lone_short_tail_stops_corroborating_beyond_the_burst_gap() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let mut run = WindowsPasteRun::default();
    let clipboard = "alpha\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    review_pr91_deliver_batch(&mut run, &editor, "alpha", false, now);
    assert!(
        !windows_enter_in_paste(
            &run,
            false,
            Some(clipboard),
            now + std::time::Duration::from_millis(25)
        ),
        "beyond machine speed the Enter is the user's, even if the clipboard matches"
    );
}

#[test]
fn round6_pr91_carried_last_batch_corroborates_only_within_the_burst_gap() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let mut run = WindowsPasteRun::default();
    review_pr91_deliver_batch(&mut run, &editor, "你好", false, now);
    let commit_at = now + std::time::Duration::from_millis(50);
    review_pr91_deliver_batch(&mut run, &editor, "我们", false, commit_at);
    let clipboard = "我们\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    assert!(
        windows_enter_in_paste(
            &run,
            false,
            Some(clipboard),
            commit_at + std::time::Duration::from_millis(15)
        ),
        "a carried history must not change the machine-speed window of the last batch"
    );
    assert!(
        !windows_enter_in_paste(
            &run,
            false,
            Some(clipboard),
            commit_at + std::time::Duration::from_millis(25)
        ),
        "the last batch alone is back to a typed Enter once machine speed is exceeded"
    );
}

// Round-7 review probe for PR #91: concatenating short newline-less IME
// batches must not give the accumulated candidate full carry eligibility.
#[test]
fn next_review_pr91_accumulated_ime_batches_must_not_swallow_submit() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let mut run = WindowsPasteRun::default();
    review_pr91_deliver_batch(&mut run, &editor, "你好", false, now);
    let second_at = now + std::time::Duration::from_millis(50);
    review_pr91_deliver_batch(&mut run, &editor, "我们", false, second_at);
    let entered_at = second_at + std::time::Duration::from_millis(80);
    assert_eq!(editor.get_text(), "你好我们");
    assert!(
        run.tail.as_ref().unwrap().lone_short,
        "the concatenated IME shape is lone-short as a whole"
    );
    let clipboard = "你好我们\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    assert!(
        !windows_enter_in_paste(&run, false, Some(clipboard), entered_at),
        "a concatenated pair of short IME commits must not become paste evidence at 80 ms"
    );
}

// Flanking probe: the same concatenated shape corroborates only while an
// Enter could trail the second batch at machine speed.
#[test]
fn round7_pr91_concatenated_short_batches_corroborate_only_at_machine_speed() {
    let now = std::time::Instant::now();
    let editor = rpi_tui::Editor::simple();
    let mut run = WindowsPasteRun::default();
    let clipboard = "你好我们\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
    review_pr91_deliver_batch(&mut run, &editor, "你好", false, now);
    let second_at = now + std::time::Duration::from_millis(50);
    review_pr91_deliver_batch(&mut run, &editor, "我们", false, second_at);
    assert!(
        windows_enter_in_paste(
            &run,
            false,
            Some(clipboard),
            second_at + std::time::Duration::from_millis(15)
        ),
        "a payload split into two newline-less batches keeps its newline at machine speed"
    );
    assert!(
        !windows_enter_in_paste(
            &run,
            false,
            Some(clipboard),
            second_at + std::time::Duration::from_millis(25)
        ),
        "beyond machine speed the accumulated newline-less text is IME-shaped, not paste evidence"
    );
}

/// The fullscreen drag-selection extraction must use the real terminal
/// geometry, not a hardcoded 80-column render.
///
/// Fail-on-old record: against `fcfc931` this probe panicked with
/// `40-column selection leaked the 80-column layout's rows` -- the old
/// `extract_selected_text` rendered the chat at `render(80)` and returned
/// every row of the 80-column layout for a 40-column screen selection.
/// The new selection pipeline (rpi-tui `selection`) slices the cached
/// document at the live width, so the same drag returns only the wrapped
/// rows the user saw.
#[test]
fn fullscreen_selection_extraction_uses_the_real_terminal_width() {
    use crate::interactive_tui::selection_probe::{probe_extract_at_width, SelectionGeometry};
    let geometry = SelectionGeometry {
        width: 40,
        rows: 12,
    };
    // Select the first two wrapped rows of a long message on a 40-column
    // screen: must NOT leak rows that only exist in an 80-column layout.
    let text = probe_extract_at_width(
        "0123456789012345678901234567890123456789012345678901234567890123456789012345678",
        "tail marker row",
        geometry,
    )
    .expect("selection produced text");
    assert!(
        !text.contains("tail marker"),
        "40-column selection leaked the 80-column layout's rows: {text:?}"
    );
}

/// The reverse-video highlight must reach the terminal during the drag (the
/// user sees what they are selecting), while a plain click must neither
/// highlight nor leave any selection to copy.
#[test]
fn fullscreen_drag_paints_highlight_and_click_does_not() {
    use crate::interactive_tui::selection_probe::{probe_click, probe_drag, SelectionGeometry};
    let geometry = SelectionGeometry {
        width: 40,
        rows: 12,
    };
    let long_line =
        "0123456789012345678901234567890123456789012345678901234567890123456789012345678";

    // Drag: the Drag re-render writes the inverse-video span.
    let (drag_frame, drag_text) = probe_drag(long_line, "tail marker row", geometry);
    assert!(
        drag_frame.contains("\x1b[7m"),
        "drag must paint the reverse-video highlight: {drag_frame:?}"
    );
    assert!(drag_text.is_some(), "drag must produce extractable text");

    // Click: no inverse-video ever written, no text selected.
    let (click_frame, click_text) = probe_click(long_line, "tail marker row", geometry);
    assert!(
        !click_frame.contains("\x1b[7m"),
        "a plain click must not paint a highlight: {click_frame:?}"
    );
    assert!(click_text.is_none(), "a plain click must not select text");
}

// ==== Reviewer probes for PR #106 (carried verbatim; the Windows ones call
// the real state/paste-run/editor methods in the key loop's NEW order). ====

fn review_pr106_slash_state() -> Arc<TuiState> {
    let state = test_tui_state();
    state.autocomplete.set_provider(Arc::new(
        SlashCommandAutocompleteProvider::with_default_commands(),
    ));
    state
}

#[test]
fn review_pr106_explicitly_selecting_row_zero_must_accept_it() {
    let state = review_pr106_slash_state();
    let editor = state.editor.clone();
    editor.set_text("/c");
    editor.set_cursor(0, 2);
    assert!(navigate_autocomplete(&state, &editor, 1));
    assert!(navigate_autocomplete(&state, &editor, -1));
    assert_eq!(*state.autocomplete_selection.lock().unwrap(), 0);
    assert_eq!(
        enter_completion_action(&state, &editor),
        Some(EnterCompletionAction::ApplyAndSubmit),
        "Down then Up explicitly selected the first row; it is not a fresh ambiguous popup"
    );
}

#[test]
fn review_pr106_windows_queued_prefix_enter_must_submit_the_completed_command() {
    let state = review_pr106_slash_state();
    let editor = state.editor.clone();
    let submitted = Arc::new(Mutex::new(Vec::<String>::new()));
    let submitted_for_callback = submitted.clone();
    editor.on_submit(Arc::new(move |text| {
        submitted_for_callback
            .lock()
            .unwrap()
            .push(text.to_string());
    }));
    let now = std::time::Instant::now();
    let mut run = WindowsPasteRun::default();
    assert!(review_pr91_deliver_batch(
        &mut run, &editor, "/clea", true, now
    ));
    assert_eq!(run.text, "/clea");
    assert!(editor.get_text().is_empty());
    // Follow the production key-loop ordering at the NEW head: Windows Enter
    // classification/flush FIRST, then the completion hook, then the editor.
    assert!(!windows_enter_in_paste(&run, false, None, now));
    run.flush(&editor, now);
    // The production routing entry point: one resolution, the same snapshot
    // applied, `true` only for an `@`-path completion.
    assert!(
        !route_enter_completion(&state, &editor),
        "a slash command must not consume Enter as a path completion"
    );
    editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        *submitted.lock().unwrap(),
        vec!["/clear".to_string()],
        "the completion hook must see the buffered prefix before the submitting Enter"
    );
}

#[test]
fn review_pr106_windows_pasted_interior_enter_must_not_rewrite_the_draft() {
    let state = review_pr106_slash_state();
    let editor = state.editor.clone();
    let now = std::time::Instant::now();
    let mut run = WindowsPasteRun::default();
    review_pr91_deliver_batch(&mut run, &editor, "/clea", false, now);
    assert_eq!(editor.get_text(), "/clea");
    // Production ordering at the NEW head: an interior pasted Enter is
    // classified BEFORE the completion hook can look at the draft.
    assert!(windows_enter_in_paste(&run, true, None, now));
    run.feed('\n', now);
    run.feed('X', now);
    run.flush(&editor, now);
    assert_eq!(
        editor.get_text(),
        "/clea\nX",
        "an interior pasted Enter must be classified before completion can rewrite clipboard text"
    );
}

#[test]
fn rereview_pr106_enter_in_middle_of_complete_command_must_preserve_the_command() {
    let state = review_pr106_slash_state();
    let editor = state.editor.clone();
    let submitted = Arc::new(Mutex::new(Vec::<String>::new()));
    let capture = submitted.clone();
    editor.on_submit(Arc::new(move |text| {
        capture.lock().unwrap().push(text.to_string());
    }));
    editor.set_text("/clear");
    // Move the caret back two columns, without changing any text.
    editor.set_cursor(0, 4); // /cle|ar
    let completion = resolve_enter_completion(&state, &editor).expect("a decision");
    assert_eq!(completion.action, EnterCompletionAction::ApplyAndSubmit);
    assert!(completion.apply(&state, &editor));
    editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        *submitted.lock().unwrap(),
        vec!["/clear".to_string()],
        "Enter with the caret inside an already complete command must not duplicate its suffix"
    );
}

/// Self-review probe: the whole-token resolution must not silently DROP the
/// text the user typed after the caret. With `/clearXYZ` and the caret at
/// column 4 the prefix `/cle` still completes to `/clear`, but the token as a
/// whole does not — so Enter must submit the text exactly as typed instead
/// of rewriting it to `/clear` (which would lose `XYZ`).
#[test]
fn rereview_pr106_enter_inside_an_unknown_token_keeps_the_typed_text() {
    let state = review_pr106_slash_state();
    let editor = state.editor.clone();
    let submitted = Arc::new(Mutex::new(Vec::<String>::new()));
    let capture = submitted.clone();
    editor.on_submit(Arc::new(move |text| {
        capture.lock().unwrap().push(text.to_string());
    }));
    editor.set_text("/clearXYZ");
    editor.set_cursor(0, 4); // /cle|arXYZ
    assert_eq!(
        enter_completion_action(&state, &editor),
        Some(EnterCompletionAction::Submit),
        "a token that does not complete as a whole must be submitted as typed"
    );
    let completion = resolve_enter_completion(&state, &editor).expect("a decision");
    assert!(
        !completion.apply(&state, &editor),
        "nothing may be applied when the whole token does not complete"
    );
    assert_eq!(editor.get_text(), "/clearXYZ");
}

/// Self-review probe: the same whole-token rule applies when the caret is
/// inside a token that ends the input. `/clea` + `r` is the token `/clear`,
/// so Enter must complete it to `/clear` — the same result as typing
/// `/clear` and pressing Enter.
#[test]
fn rereview_pr106_enter_inside_a_token_completes_the_whole_token() {
    let state = review_pr106_slash_state();
    let editor = state.editor.clone();
    editor.set_text("/clear");
    editor.set_cursor(0, 4); // /cle|ar
    let completion = resolve_enter_completion(&state, &editor).expect("a decision");
    assert_eq!(completion.action, EnterCompletionAction::ApplyAndSubmit);
    assert!(completion.apply(&state, &editor));
    assert_eq!(
        editor.get_text(),
        "/clear",
        "the whole token is completed, not just the prefix before the caret"
    );
}

/// Self-review probe: a command with a space after it is two tokens. With
/// the caret inside the first token (`/cle|ar now`) only the command token
/// is completed — the trailing word must survive untouched.
#[test]
fn rereview_pr106_enter_inside_a_command_token_keeps_the_following_word() {
    let state = review_pr106_slash_state();
    let editor = state.editor.clone();
    editor.set_text("/clear now");
    editor.set_cursor(0, 4); // /cle|ar now
    let completion = resolve_enter_completion(&state, &editor).expect("a decision");
    assert_eq!(completion.action, EnterCompletionAction::ApplyAndSubmit);
    assert!(completion.apply(&state, &editor));
    assert_eq!(
        editor.get_text(),
        "/clear now",
        "only the command token is replaced; the next word is untouched"
    );
}

// ==== Reviewer probes for PR #106, round 3 (the two-lookup flow they model
// is gone: the decision and the replacement are one snapshot, applied by
// `route_enter_completion` — so the probes take the snapshot and apply it,
// which is exactly what the key loop does). ====

/// `@<root>/nested/repo` + Enter must complete the basename and KEEP the
/// directory: the path provider spans the whole path prefix but returns a
/// basename, so replacing its span would point the reference at a different
/// file (`@report.txt`).
#[test]
fn final_review_pr106_enter_path_completion_must_preserve_the_parent_directory() {
    let temporary = tempfile::tempdir().unwrap();
    let nested = temporary.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let completed = nested.join("report.txt");
    std::fs::write(&completed, "review fixture").unwrap();
    assert!(!temporary.path().join("report.txt").exists());
    let prefix = format!("@{}", nested.join("repo").display());
    let expected = format!("@{} ", completed.display());
    let state = test_tui_state();
    state
        .autocomplete
        .set_provider(Arc::new(FilePathAutocompleteProvider::with_root(
            temporary.path().to_path_buf(),
        )));
    let editor = state.editor.clone();
    editor.set_text(&prefix);
    set_editor_cursor_offset(&editor, &prefix, prefix.len());
    let completion = resolve_enter_completion(&state, &editor).expect("a decision");
    assert_eq!(completion.action, EnterCompletionAction::ApplyOnly);
    assert!(completion.apply(&state, &editor));
    assert_eq!(
        editor.get_text(),
        expected,
        "Enter must complete the basename without discarding the directory from the file reference"
    );
}

/// A path candidate that disappears between the decision and the apply must
/// not turn the decided `ApplyOnly` into a submit of the unfinished prompt:
/// the snapshot is applied as decided, and an `@`-path decision consumes the
/// Enter independently of the apply result.
#[test]
fn final_review_pr106_disappearing_file_candidate_must_not_turn_enter_into_submit() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("notes.txt");
    std::fs::write(&path, "review fixture").unwrap();
    let state = test_tui_state();
    state
        .autocomplete
        .set_provider(Arc::new(FilePathAutocompleteProvider::with_root(
            temporary.path().to_path_buf(),
        )));
    let editor = state.editor.clone();
    editor.set_text("@not");
    editor.set_cursor(0, 4);
    let submitted = Arc::new(Mutex::new(Vec::<String>::new()));
    let capture = submitted.clone();
    editor.on_submit(Arc::new(move |text| {
        capture.lock().unwrap().push(text.to_string());
    }));
    // The decision is taken ONCE; the key loop keeps this snapshot.
    let completion = resolve_enter_completion(&state, &editor).expect("a decision");
    assert_eq!(completion.action, EnterCompletionAction::ApplyOnly);
    // Model a real directory change after the decision, before the apply. No
    // provider stub is used.
    std::fs::remove_file(&path).unwrap();
    // The production routing applies the SAME snapshot and consumes the Enter
    // for an `@`-path completion; the key never reaches the editor.
    let consumed = {
        completion.apply(&state, &editor);
        completion.consumes_enter()
    };
    if !consumed {
        editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }
    assert!(
        submitted.lock().unwrap().is_empty(),
        "a disappearing path candidate must not cause ApplyOnly to submit the unfinished prompt"
    );
    assert_eq!(
        editor.get_text(),
        "@notes.txt ",
        "the decided candidate is applied from the snapshot, not re-resolved"
    );
}

/// Self-review probe: the key loop's actual entry point (`route_enter_completion`)
/// resolves once, completes the basename, and consumes the Enter so the prompt
/// is not submitted.
#[test]
fn final_review_pr106_path_completion_route_consumes_the_enter() {
    let temporary = tempfile::tempdir().unwrap();
    std::fs::write(temporary.path().join("notes.txt"), "review fixture").unwrap();
    let state = test_tui_state();
    state
        .autocomplete
        .set_provider(Arc::new(FilePathAutocompleteProvider::with_root(
            temporary.path().to_path_buf(),
        )));
    let editor = state.editor.clone();
    editor.set_text("@not");
    editor.set_cursor(0, 4);
    let submitted = Arc::new(Mutex::new(Vec::<String>::new()));
    let capture = submitted.clone();
    editor.on_submit(Arc::new(move |text| {
        capture.lock().unwrap().push(text.to_string());
    }));
    assert!(
        route_enter_completion(&state, &editor),
        "an @-path completion consumes the Enter"
    );
    assert_eq!(editor.get_text(), "@notes.txt ");
    assert!(
        submitted.lock().unwrap().is_empty(),
        "a path completion must never submit the prompt"
    );
}

/// Self-review probe: the path segment rule matches the command-token rule —
/// the whole segment around the caret is replaced (no duplicated suffix), and
/// a segment that does not complete keeps the typed text instead of dropping
/// it.
#[test]
fn final_review_pr106_path_segment_covers_the_text_after_the_caret() {
    let temporary = tempfile::tempdir().unwrap();
    let nested = temporary.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let completed = nested.join("report.txt");
    std::fs::write(&completed, "review fixture").unwrap();
    let state = test_tui_state();
    state
        .autocomplete
        .set_provider(Arc::new(FilePathAutocompleteProvider::with_root(
            temporary.path().to_path_buf(),
        )));
    let editor = state.editor.clone();
    // Caret inside the basename: `.../nested/repo|rt.txt`.
    let prefix = format!("@{}", nested.join("repo").display());
    let text = format!("{prefix}rt.txt");
    editor.set_text(&text);
    set_editor_cursor_offset(&editor, &text, prefix.len());
    assert!(
        route_enter_completion(&state, &editor),
        "a path completion consumes the Enter"
    );
    assert_eq!(
        editor.get_text(),
        format!("@{} ", completed.display()),
        "the whole basename segment is completed, not just the part before the caret"
    );

    // A segment that does not complete as a whole keeps the typed text: the
    // completion must not silently drop `zz` after the caret.
    let unknown = format!("{prefix}zz.txt");
    editor.set_text(&unknown);
    set_editor_cursor_offset(&editor, &unknown, prefix.len());
    assert!(
        !route_enter_completion(&state, &editor),
        "a segment that does not complete falls through to the normal submit"
    );
    assert_eq!(
        editor.get_text(),
        unknown,
        "nothing may be rewritten when the whole segment does not complete"
    );
}

#[test]
fn merge_review_pr106_enter_after_directory_completion_must_not_duplicate_the_directory() {
    let temporary = tempfile::tempdir().unwrap();
    let nested = temporary.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join("report.txt"), "review fixture").unwrap();
    assert!(!nested.join("nested").exists());
    let state = test_tui_state();
    state
        .autocomplete
        .set_provider(Arc::new(FilePathAutocompleteProvider::with_root(
            temporary.path().to_path_buf(),
        )));
    let editor = state.editor.clone();
    let prefix = format!("@{}", temporary.path().join("nes").display());
    editor.set_text(&prefix);
    set_editor_cursor_offset(&editor, &prefix, prefix.len());
    assert!(route_enter_completion(&state, &editor));
    let first = editor.get_text();
    assert!(
        first.ends_with("nested/"),
        "first directory completion: {first}"
    );
    refresh_autocomplete(&state, &editor);
    // A directory completion leaves its separator at the caret, so the next
    // Enter should operate inside that directory, not insert its name again.
    let consumed = route_enter_completion(&state, &editor);
    let second = editor.get_text();
    let reference = second.trim().strip_prefix('@').unwrap();
    assert!(
        std::path::Path::new(reference).exists(),
        "Enter after a directory completion must retain a valid reference; first={first:?}, second={second:?}, consumed={consumed}"
    );
}

/// Self-review probe: a directory completion leaves its separator at the
/// caret, so the NEXT Enter completes INSIDE that directory instead of
/// inserting the directory's own name again (`…/nested/nested/`, a reference
/// that does not exist). Pins the exact text and the consumed key.
///
/// The path provider marks a directory with `/` on EVERY platform (the
/// upstream TS convention — `toDisplayPath` normalizes `\` to `/`; the
/// upstream-derived provider probe pins `vec!["nested/"]`), while the part
/// of the path the user typed keeps the platform separator. The completed
/// text therefore mixes the two glyphs, and the raw string differs per
/// platform even though it names the same file. The assertion normalizes
/// the separators, so it pins the reference itself and not the glyph.
#[test]
fn final_review_pr106_enter_after_directory_completion_completes_inside_it() {
    let temporary = tempfile::tempdir().unwrap();
    let nested = temporary.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let report = nested.join("report.txt");
    std::fs::write(&report, "review fixture").unwrap();
    let state = test_tui_state();
    state
        .autocomplete
        .set_provider(Arc::new(FilePathAutocompleteProvider::with_root(
            temporary.path().to_path_buf(),
        )));
    let editor = state.editor.clone();
    let prefix = format!(
        "@{}{}nes",
        temporary.path().display(),
        std::path::MAIN_SEPARATOR
    );
    editor.set_text(&prefix);
    set_editor_cursor_offset(&editor, &prefix, prefix.len());
    assert!(
        route_enter_completion(&state, &editor),
        "the directory completion consumes the Enter"
    );
    let first = editor.get_text();
    assert_eq!(
        normalize_path_separators(&first),
        normalize_path_separators(&format!("@{}/", nested.display())),
        "the first Enter completes the directory and leaves its separator at the caret"
    );
    // The provider marks a directory with `/` on EVERY platform (upstream TS
    // normalizes `\` to `/`), so the completed reference ends in a forward
    // slash even where the typed path used the platform separator. The
    // convention is the same on Linux and Windows — pinned here so a change
    // to it cannot pass on one platform and fail on the other.
    assert!(
        first.ends_with("nested/"),
        "the directory marker is `/` on every platform: {first:?}"
    );
    refresh_autocomplete(&state, &editor);
    assert!(
        route_enter_completion(&state, &editor),
        "the completion inside the directory consumes the Enter too"
    );
    let second = editor.get_text();
    assert_eq!(
        normalize_path_separators(&second),
        normalize_path_separators(&format!("@{} ", report.display())),
        "the second Enter completes a child of the selected directory, not the directory itself"
    );
    // The reference must name the file that exists — the invariant behind
    // the text comparison, independent of any separator glyph.
    let reference = second.trim().strip_prefix('@').expect("an @-reference");
    assert!(
        std::path::Path::new(reference).exists(),
        "the completed reference must name the real file: {second:?}"
    );
    assert!(
        !nested.join("nested").exists(),
        "no completion may invent a directory named after its own parent"
    );
}

/// Normalize the path separators of a reference so an assertion compares the
/// path a completion names, not the separator glyph it was typed with.
///
/// The path provider marks a directory with `/` on every platform, so a
/// completed reference mixes the provider's separator with the platform one
/// the user typed. The normalized form is the same string on every platform.
fn normalize_path_separators(reference: &str) -> String {
    reference.replace('\\', "/")
}

/// Self-review probe: the recorded Windows reproduction, kept in the tree so
/// the platform-dependent comparison cannot come back unnoticed.
///
/// On the Windows checkout the completed reference ended in the provider's
/// `/` while the typed path used the platform `\`:
/// `@D:\Temp\...\nested/` (actual) vs `@D:\Temp\...\nested\` (expected) —
/// the same file, two glyphs. The provider's `/` marker is the upstream
/// convention and is pre-existing (it is in `rpi-tui` before this PR), so
/// the comparison is the thing that has to be separator-agnostic.
#[test]
fn final_review_pr106_path_reference_comparison_is_separator_agnostic() {
    // The exact shapes the Windows checkout produced.
    let actual = "@D:\\Temp\\pr106\\nested/";
    let expected = "@D:\\Temp\\pr106\\nested\\";
    assert_ne!(
        actual, expected,
        "the raw texts differ: that is the platform difference the comparison must absorb"
    );
    assert_eq!(
        normalize_path_separators(actual),
        normalize_path_separators(expected),
        "the normalized references are the same reference"
    );
    // The same holds for the file the reference ends at.
    let actual_file = "@D:\\Temp\\pr106\\nested/report.txt ";
    let expected_file = "@D:\\Temp\\pr106\\nested\\report.txt ";
    assert_eq!(
        normalize_path_separators(actual_file),
        normalize_path_separators(expected_file),
        "the normalized file references are the same reference"
    );
}

/// Self-review probe: the completed reference must NAME THE FILE, on every
/// platform — the invariant the text comparison cannot express.
///
/// The provider marks a directory with `/` on every platform, so on Windows
/// the completed reference mixes the typed `\` with the provider's `/`. The
/// reference is resolved as a path (the OS accepts both separators on
/// Windows), so the file it names is the same on every platform. A probe
/// that only compared the raw text could pass on Linux and fail on Windows
/// while the feature worked; this one compares the resolved path.
#[test]
fn final_review_pr106_completed_path_reference_resolves_to_the_same_file() {
    let temporary = tempfile::tempdir().unwrap();
    let nested = temporary.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let report = nested.join("report.txt");
    std::fs::write(&report, "review fixture").unwrap();
    let state = test_tui_state();
    state
        .autocomplete
        .set_provider(Arc::new(FilePathAutocompleteProvider::with_root(
            temporary.path().to_path_buf(),
        )));
    let editor = state.editor.clone();
    let prefix = format!(
        "@{}{}nes",
        temporary.path().display(),
        std::path::MAIN_SEPARATOR
    );
    editor.set_text(&prefix);
    set_editor_cursor_offset(&editor, &prefix, prefix.len());
    assert!(route_enter_completion(&state, &editor));
    refresh_autocomplete(&state, &editor);
    assert!(route_enter_completion(&state, &editor));
    let text = editor.get_text();
    let reference = text.trim().strip_prefix('@').expect("an @-reference");
    // The reference resolves to the file that exists — the invariant behind
    // the text comparison, independent of any separator glyph.
    let resolved = std::path::Path::new(reference);
    assert!(
        resolved.exists(),
        "the completed reference must name the real file: {text:?}"
    );
    assert_eq!(
        resolved.file_name().and_then(|name| name.to_str()),
        Some("report.txt"),
        "the completed reference must name the child file: {text:?}"
    );
    assert_eq!(
        resolved.parent().and_then(|parent| parent.file_name()),
        Some(std::ffi::OsStr::new("nested")),
        "the completed reference must stay inside the selected directory: {text:?}"
    );
}

/// Self-review probe: Tab applies the same basename-segment span as Enter.
/// The path provider returns a basename while spanning the whole path prefix,
/// so accepting its span dropped the directory: `@/abs/nested/repo` + Tab
/// became `@report.txt`, a reference to a different file. A segment that does
/// not complete as a whole is left exactly as typed.
#[test]
fn final_review_pr106_tab_path_completion_must_preserve_the_parent_directory() {
    let temporary = tempfile::tempdir().unwrap();
    let nested = temporary.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let report = nested.join("report.txt");
    std::fs::write(&report, "review fixture").unwrap();
    assert!(!temporary.path().join("report.txt").exists());
    let state = test_tui_state();
    state
        .autocomplete
        .set_provider(Arc::new(FilePathAutocompleteProvider::with_root(
            temporary.path().to_path_buf(),
        )));
    let editor = state.editor.clone();
    let prefix = format!("@{}{}repo", nested.display(), std::path::MAIN_SEPARATOR);
    editor.set_text(&prefix);
    set_editor_cursor_offset(&editor, &prefix, prefix.len());
    assert!(
        accept_top_suggestion(&state, &editor),
        "Tab accepts the candidate"
    );
    assert_eq!(
        editor.get_text(),
        format!("@{} ", report.display()),
        "Tab must complete the basename without discarding the directory"
    );

    // The candidate does not complete the whole segment, so nothing is
    // rewritten (the typed suffix after the caret survives).
    let suffix = format!("{prefix}zz.txt");
    editor.set_text(&suffix);
    set_editor_cursor_offset(&editor, &suffix, prefix.len());
    assert!(
        !accept_top_suggestion(&state, &editor),
        "an uncompletable segment is not rewritten"
    );
    assert_eq!(editor.get_text(), suffix);
}
