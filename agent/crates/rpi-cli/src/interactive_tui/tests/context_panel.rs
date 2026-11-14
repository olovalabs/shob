use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rpi_harness::types::{AgentHarnessResources, PromptTemplate, Skill};

fn selector(state: &Arc<TuiState>) -> Arc<SearchableSelectList> {
    let active = state.active_selector.lock().unwrap().clone().unwrap();
    assert!(active.1 == SelectorKind::Context);
    match active.0 {
        SelectorView::Searchable(view) => view,
        _ => panic!("context must be searchable"),
    }
}

fn terminal() -> Arc<TuiAltScreen> {
    // Do not start a real terminal: only exercise component and key routing.
    Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        true,
        None,
    ))
}

#[test]
fn context_is_in_production_autocomplete_and_help() {
    let registry = build_builtin_registry();
    let entries = registry.visible_entries();
    assert!(entries.iter().any(|entry| entry.name == "/context"));
    assert!(!registry.find("/context").unwrap().description().is_empty());
    let state = test_tui_state();
    state
        .autocomplete
        .set_provider(Arc::new(SlashCommandAutocompleteProvider::new(entries)));
    state.editor.set_text("/cont");
    state.editor.set_cursor(0, 5);
    refresh_autocomplete(&state, &state.editor);
    let output = strip_ansi(&state.autocomplete_container.render(120).join("\n"));
    assert!(output.contains("/context"), "{output}");
    let chat = Arc::new(Container::new());
    add_help_message(&chat);
    assert!(strip_ansi(&chat.render(120).join("\n")).contains("/context"));
}

#[test]
fn context_cancel_and_confirm_restore_editor_without_changing_chat_or_resources() {
    let state = test_tui_state();
    let tui = terminal();
    let container = Arc::new(Container::new());
    let editor = state.editor.clone();
    editor.set_text("draft that must survive");
    editor.set_focused(true);
    container.add_child(editor.clone());
    add_note_message(&state.chat_container, "existing conversation");
    let chat_before = state.chat_container.render(120);
    let resources = AgentHarnessResources {
        context_files: vec!["/project/AGENTS.md".into()],
        ..Default::default()
    };
    let snapshot = resources.clone();
    for key in [KeyCode::Esc, KeyCode::Enter] {
        open_context_selector(&state, &container, &editor, &tui, &resources);
        let view = selector(&state);
        assert!(!editor.is_focused());
        let output = strip_ansi(&container.render(120).join("\n"));
        assert!(output.contains("/project/AGENTS.md"));
        assert!(output.contains("Esc/Enter close"));
        view.handle_key(KeyEvent::new(key, KeyModifiers::NONE));
        assert!(!state.selector_open());
        assert!(editor.is_focused());
        assert_eq!(container.child_count(), 1);
        assert!(strip_ansi(&container.render(120).join("\n")).contains("draft that must survive"));
        assert!(!strip_ansi(&container.render(120).join("\n")).contains("/project/AGENTS.md"));
        assert_eq!(editor.get_text(), "draft that must survive");
        assert_eq!(state.chat_container.render(120), chat_before);
        assert_eq!(resources, snapshot);
    }
}

#[test]
fn context_scrolls_filters_and_can_cancel_with_no_matches() {
    let state = test_tui_state();
    let tui = terminal();
    let container = Arc::new(Container::new());
    let resources = AgentHarnessResources {
        context_files: (0..30)
            .map(|n| format!("/project/context-{n:02}.md"))
            .collect(),
        ..Default::default()
    };
    open_context_selector(&state, &container, &state.editor, &tui, &resources);
    let view = selector(&state);
    for _ in 0..31 {
        view.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    }
    assert!(strip_ansi(&container.render(120).join("\n")).contains("context-29.md"));
    view.set_search_text("context-29");
    let output = strip_ansi(&container.render(120).join("\n"));
    assert!(output.contains("context-29.md"));
    assert!(!output.contains("context-00.md"));
    view.set_search_text("definitely-not-a-resource");
    view.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!state.selector_open());
    assert_eq!(state.chat_container.child_count(), 0);
}

#[test]
fn context_reopens_with_refreshed_or_empty_resources_without_stale_paths() {
    let state = test_tui_state();
    let tui = terminal();
    let container = Arc::new(Container::new());
    for paths in [
        vec!["/old/SYSTEM.md".into()],
        vec!["/new/CLAUDE.md".into()],
        vec![],
    ] {
        let resources = AgentHarnessResources {
            context_files: paths.clone(),
            ..Default::default()
        };
        open_context_selector(&state, &container, &state.editor, &tui, &resources);
        let output = strip_ansi(&container.render(120).join("\n"));
        if let Some(path) = paths.first() {
            assert!(output.contains(path));
        } else {
            assert!(output.contains("no context files loaded"));
        }
        if !paths.iter().any(|p| p.contains("/old/")) {
            assert!(!output.contains("/old/SYSTEM.md"));
        }
        selector(&state).handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!state.selector_open());
        assert_eq!(state.chat_container.child_count(), 0);
    }
}

#[tokio::test]
async fn context_command_does_not_append_chat_queue_usage_or_clear_session() {
    struct RestoreConfig(Option<std::ffi::OsString>);
    impl Drop for RestoreConfig {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var(crate::config::CONFIG_DIR_ENV, value),
                None => std::env::remove_var(crate::config::CONFIG_DIR_ENV),
            }
        }
    }
    let _guard = crate::config::test_support::env_lock().lock().unwrap();
    let _restore = RestoreConfig(std::env::var_os(crate::config::CONFIG_DIR_ENV));
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var(crate::config::CONFIG_DIR_ENV, tmp.path());
    let resolved = crate::provider::resolve(
        Some("anthropic"),
        Some(crate::provider::DEFAULT_MODEL_ID),
        None,
        Some("test-key"),
        None,
    )
    .unwrap();
    let args = crate::args::Args {
        trust_override: Some(false),
        no_session: true,
        no_extensions: true,
        no_skills: true,
        no_prompt_templates: true,
        no_context_files: true,
        system_prompt: Some("stable model context".into()),
        ..Default::default()
    };
    let (harness, _events, reload_context) =
        crate::session::build(&resolved, &args, tmp.path(), false)
            .await
            .unwrap();
    harness
        .session()
        .append_message(user_message_with_images("keep this message", vec![]))
        .await
        .unwrap();
    let session = Arc::new(crate::agent_session::AgentSession::new(
        harness,
        vec![],
        vec![],
        tmp.path(),
    ));
    let entries_before = serde_json::to_value(session.entries().await.unwrap()).unwrap();
    let prompt_before = session.harness().get_system_prompt().await.unwrap();
    let resources_before = session.harness().get_resources().await.unwrap();
    let state = test_tui_state();
    let editor_container = Arc::new(Container::new());
    editor_container.add_child(state.editor.clone());
    add_note_message(&state.chat_container, "keep this transcript");
    let chat_before = state.chat_container.render(120);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let ctx = CommandContext {
        chat: state.chat_container.clone(),
        tui: terminal(),
        tx,
        editor: state.editor.clone(),
        editor_container,
        lane: session.harness().lane("main"),
        model_catalog: Arc::new(std::sync::RwLock::new(vec![])),
        cwd: tmp.path().to_path_buf(),
        resources: Arc::new(Mutex::new(resources_before.clone())),
        startup_context: None,
        reload_context: Arc::new(reload_context),
        session: session.clone(),
        streaming: false,
        state: state.clone(),
    };
    let registry = build_builtin_registry();
    registry.find("/context").unwrap().execute(&ctx, "");
    assert!(state.selector_open());
    assert!(
        matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "opening /context must not queue a late transcript note"
    );
    selector(&state).handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!state.selector_open());
    assert_eq!(state.chat_container.render(120), chat_before);
    assert_eq!(
        serde_json::to_value(session.entries().await.unwrap()).unwrap(),
        entries_before
    );
    assert_eq!(
        session.harness().get_system_prompt().await.unwrap(),
        prompt_before
    );
    assert_eq!(
        session.harness().get_resources().await.unwrap(),
        resources_before
    );
    assert_eq!(*ctx.resources.lock().unwrap(), resources_before);
    // The dedicated /usage command still owns the async statistics path.
    registry.find("/usage").unwrap().execute(&ctx, "");
    assert!(matches!(rx.try_recv(), Ok(TuiMessage::ShowUsage)));
}

#[test]
fn context_retains_skills_templates_and_empty_resource_guidance() {
    let resources = AgentHarnessResources {
        skills: Some(vec![Skill {
            name: "release".into(),
            description: "Publish the workspace".into(),
            content: "private skill body".into(),
            file_path: "/skills/release/SKILL.md".into(),
            disable_model_invocation: Some(true),
        }]),
        prompt_templates: Some(vec![PromptTemplate {
            name: "review".into(),
            description: Some("Review changes".into()),
            content: "private template body".into(),
        }]),
        ..Default::default()
    };
    let output = context_panel_lines(&resources).join("\n");
    assert!(output.contains("release [hidden] — Publish the workspace"));
    assert!(output.contains("/review — Review changes"));
    assert!(!output.contains("private"));
    let output = context_panel_lines(&AgentHarnessResources::default()).join("\n");
    assert!(output.contains("no context files loaded"));
    assert!(output.contains("none discovered"));
    assert!(output.contains(".rpi/prompts/"));
}
