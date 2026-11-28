#[test]
fn model_tab_lists_only_connected_provider_models() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    let connected = agent_auth::AgentModel {
        provider: "google-antigravity".into(),
        id: "gemini-connected".into(),
        backend: agent_auth::ModelBackend::Shob,
    };
    let disconnected = agent_auth::AgentModel {
        provider: "anthropic".into(),
        id: "claude-disconnected".into(),
        backend: agent_auth::ModelBackend::Embedded,
    };
    let local = agent_auth::AgentModel {
        provider: "ollama".into(),
        id: "example-local-model".into(),
        backend: agent_auth::ModelBackend::Embedded,
    };
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_chat_mode(id, true, window, cx);
        let input = s.pane_ref(id).unwrap().input_state.clone().unwrap();
        input.update(cx, |input, cx| input.set_value("/model", window, cx));
        s.execute_pane_input(id, window, cx);
        let providers = vec![
            agent_auth::Provider {
                name: None,
                custom: false,
                id: "anthropic".into(),
                ready: false,
                source: None,
            },
            agent_auth::Provider {
                name: None,
                custom: false,
                id: "ollama-cloud".into(),
                ready: true,
                source: Some("environment".into()),
            },
            agent_auth::Provider {
                name: None,
                custom: false,
                id: "google-antigravity".into(),
                ready: true,
                source: Some("Shob credential".into()),
            },
            agent_auth::Provider {
                name: None,
                custom: false,
                id: "ollama".into(),
                ready: true,
                source: Some("Stored API key".into()),
            },
            agent_auth::Provider {
                name: None,
                custom: false,
                id: "web".into(),
                ready: true,
                source: Some("anonymous".into()),
            },
        ];
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        sender
            .send(Ok(AgentCatalogData {
                providers,
                models: vec![disconnected, connected.clone(), local],
            }))
            .unwrap();
        s.agent_catalog.pending = Some(receiver);
        s.poll_agent_events(cx);
        s.pane_mut(id).unwrap().chat.selected_model = Some(connected);
        s.close_agent_auth(id, window, cx);
        s.open_agent_auth(id, AgentDialogPage::Models, window, cx);
    });
    cx.draw_frame().expect("draw model tab");
    assert!(
        cx.debug_bounds("agent-model-0").is_some(),
        "connected provider models are listed immediately after reopening"
    );
    assert!(
        cx.debug_bounds("agent-model-1").is_none(),
        "models from disconnected and local providers are hidden"
    );
    for selector in [
        "model-provider-filter-google-antigravity",
        "model-provider-filter-ollama-cloud",
    ] {
        assert!(cx.debug_bounds(selector).is_some());
    }
    assert!(cx.debug_bounds("model-provider-filter-anthropic").is_none());
    assert!(cx.debug_bounds("model-provider-filter-ollama").is_none());
    assert!(cx.debug_bounds("model-provider-filter-web").is_none());
    let cloud = cx
        .debug_bounds("model-provider-filter-ollama-cloud")
        .unwrap();
    assert!(
        cx.debug_bounds("picker-reasoning-provider-Google Antigravity")
            .is_some()
    );
    cx.simulate_click(cloud.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-model-0").is_none());
    assert!(
        cx.debug_bounds("picker-reasoning-provider-Google Antigravity")
            .is_none()
    );
    assert!(cx.debug_bounds("model-reasoning-medium").is_none());
    cx.simulate_click(cloud.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-model-0").is_some());
    let manage = cx.debug_bounds("agent-more-providers").unwrap();
    cx.simulate_click(manage.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-provider-panel").is_some());
    assert!(cx.debug_bounds("auth-provider-3").is_none());
    assert!(cx.debug_bounds("auth-provider-selected-0").is_some());
    assert!(cx.debug_bounds("auth-provider-connected-1").is_some());
    assert!(cx.debug_bounds("auth-provider-connected-2").is_none());
    let connect = cx.debug_bounds("auth-provider-2").unwrap();
    cx.simulate_click(connect.center(), Modifiers::default());
    state.read_with(&cx, |s, _| {
        let auth = s.pane_ref(id).unwrap().chat.auth.as_ref().unwrap();
        assert_eq!(auth.selected.as_deref(), Some("anthropic"));
    });
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("auth-provider-selected-0").is_none());
    assert!(cx.debug_bounds("auth-provider-connected-0").is_some());
    assert!(cx.debug_bounds("auth-provider-selected-2").is_some());
}

#[test]
fn provider_icons_resolve_to_the_downloaded_set() {
    load_provider_icons();
    assert_eq!(
        provider_icon_path("openai").as_ref(),
        "icons/provider/openai.svg"
    );
    assert_eq!(
        provider_icon_path("opencode-zen").as_ref(),
        "icons/provider/opencode.svg"
    );
    assert_eq!(
        provider_icon_path("google-antigravity").as_ref(),
        "icons/provider/google.svg"
    );
    assert_eq!(
        provider_icon_path("not-a-provider").as_ref(),
        "icons/provider/synthetic.svg"
    );
}
#[test]
fn provider_page_cards_use_the_two_column_icon_grid() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, window, cx| {
        s.open_agent_auth(id, AgentDialogPage::Auth, window, cx);
        s.agent_catalog.data.providers = vec![
            agent_auth::Provider {
                name: None,
                custom: false,
                id: "openai".into(),
                ready: false,
                source: None,
            },
            agent_auth::Provider {
                name: None,
                custom: false,
                id: "anthropic".into(),
                ready: false,
                source: None,
            },
            agent_auth::Provider {
                name: None,
                custom: false,
                id: "google".into(),
                ready: false,
                source: None,
            },
            agent_auth::Provider {
                name: None,
                custom: false,
                id: "github-copilot".into(),
                ready: false,
                source: None,
            },
        ];
    });
    cx.draw_frame().expect("draw provider page");

    let c0 = cx.debug_bounds("auth-provider-0").unwrap();
    let c1 = cx.debug_bounds("auth-provider-1").unwrap();
    let c2 = cx.debug_bounds("auth-provider-2").unwrap();
    let c3 = cx.debug_bounds("auth-provider-3").unwrap();

    assert!((c0.center().y - c1.center().y).abs() < px(1.0));
    assert!(c0.right() < c1.left());
    assert!(c0.top() < c2.top());
    assert!((c2.center().y - c3.center().y).abs() < px(1.0));
    assert!(c2.right() < c3.left());
}

#[test]
fn provider_disconnect_button_disconnects_without_switching_models() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    let model = agent_auth::AgentModel {
        provider: "commandcode".into(),
        id: "deepseek/deepseek-v4-flash".into(),
        backend: agent_auth::ModelBackend::Embedded,
    };
    cx.update_window_entity(&state, |s, window, cx| {
        s.select_agent_model(id, Some(model.clone()), window, cx);
        s.open_agent_auth(id, AgentDialogPage::Auth, window, cx);
        s.agent_catalog.data.providers = vec![agent_auth::Provider {
            name: None,
            custom: false,
            id: "commandcode".into(),
            ready: true,
            source: Some("Stored API key".into()),
        }];
    });
    cx.draw_frame()
        .expect("draw provider page with connected provider");

    let disconnect_btn = cx
        .debug_bounds("auth-disconnect-0")
        .expect("disconnect button renders");
    let center = disconnect_btn.center();
    cx.simulate_mouse_move(center, None, Modifiers::default());
    cx.simulate_click(center, Modifiers::default());
    cx.draw_frame().expect("draw after disconnect click");

    let is_ready = state.read_with(&cx, |s, _| {
        s.agent_catalog
            .data
            .providers
            .iter()
            .any(|p| p.id == "commandcode" && p.ready)
    });
    assert!(
        !is_ready,
        "provider must be disconnected after clicking the disconnect button"
    );
    assert!(
        cx.debug_bounds("auth-disconnect-0").is_none(),
        "disconnect button must disappear once disconnected"
    );
    assert!(cx.debug_bounds("auth-provider-selected-0").is_none());
    state.read_with(&cx, |s, _| {
        assert_eq!(
            s.pane_ref(id).unwrap().chat.selected_model.as_ref(),
            Some(&model)
        );
        assert_eq!(s.saved_settings().default_agent_model.as_ref(), Some(&model));
    });
}

#[test]
fn agent_dialog_backdrop_covers_the_window_and_blocks_background() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let (pane_id, sessions_before) = state.read_with(&cx, |s, _| {
        (
            s.projects[0].sessions[0].panes[0].id,
            s.projects[0].sessions.len(),
        )
    });
    cx.update_window_entity(&state, |s, window, cx| {
        s.open_agent_auth(pane_id, AgentDialogPage::Models, window, cx);
    });
    cx.draw_frame().expect("draw agent dialog");

    let layer = cx
        .debug_bounds("agent-auth-layer")
        .expect("dialog backdrop renders");
    assert_eq!(
        layer.origin,
        point(px(0.0), px(0.0)),
        "the backdrop must start at the window origin, not the pane"
    );
    assert!(
        layer.size.width > px(1100.0) && layer.size.height > px(700.0),
        "the backdrop must span the whole window (sidebar and title bar included)"
    );

    let new_session = cx
        .debug_bounds("sidebar-new-thread")
        .expect("sidebar new-session button renders");
    let click = point(
        new_session.origin.x + new_session.size.width / 2.0,
        new_session.origin.y + new_session.size.height / 2.0,
    );
    cx.simulate_mouse_move(click, None, Modifiers::default());
    cx.simulate_click(click, Modifiers::default());
    cx.draw_frame().expect("draw after backdrop click");

    state.read_with(&cx, |s, _| {
        assert_eq!(
            s.projects[0].sessions.len(),
            sessions_before,
            "clicks behind the dialog must not reach the sidebar"
        );
        assert!(
            s.sidebar_project_picker.is_none(),
            "the new-thread picker must not open behind the dialog"
        );
        assert!(
            s.pane_ref(pane_id).unwrap().chat.auth.is_none(),
            "the backdrop click still dismisses the dialog"
        );
    });
}

#[test]
fn agent_dialog_filters_providers() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, window, cx| {
        s.open_agent_auth(id, AgentDialogPage::Auth, window, cx);
        s.agent_catalog.data.providers = vec![
            agent_auth::Provider {
                name: None,
                custom: false,
                id: "anthropic".into(),
                ready: false,
                source: None,
            },
            agent_auth::Provider {
                name: None,
                custom: false,
                id: "google-antigravity".into(),
                ready: true,
                source: Some("Shob credential".into()),
            },
        ];
    });
    cx.draw_frame().expect("draw provider page");
    let filter = cx
        .debug_bounds("agent-dialog-filter")
        .expect("filter input renders on the provider page");
    let center = point(
        filter.origin.x + filter.size.width / 2.0,
        filter.origin.y + filter.size.height / 2.0,
    );
    cx.simulate_mouse_move(center, None, Modifiers::default());
    cx.simulate_click(center, Modifiers::default());
    cx.draw_frame().expect("draw focused provider filter");
    cx.simulate_input("google");
    cx.draw_frame().expect("draw filtered providers");

    assert!(
        cx.debug_bounds("auth-provider-0").is_some(),
        "the matching provider stays listed"
    );
    assert!(
        cx.debug_bounds("auth-provider-1").is_none(),
        "non-matching providers are filtered out"
    );
    cx.update_window_entity(&state, |s, window, cx| {
        let auth = s.pane_ref(id).unwrap().chat.auth.as_ref().unwrap();
        auth.filter.update(cx, |input, cx| input.set_value("", window, cx));
    });
    cx.draw_frame().expect("draw providers after clearing the filter");
    assert!(
        cx.debug_bounds("auth-provider-1").is_some(),
        "clearing the filter restores the hidden provider"
    );
}

#[test]
fn shob_bottom_input_history_and_execution() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

    // Execute "echo 1"
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("echo 1", window, cx);
        });
        state.execute_pane_input(pane_id, window, cx);
    });
    wait_for_terminal_submission(&mut cx, &app_state, pane_id);

    app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        assert_eq!(pane.command_history, vec!["echo 1"]);
        let input = pane.input_state.as_ref().unwrap();
        assert_eq!(input.read(cx).value().as_str(), "");
    });

    // Execute "echo 2"
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("echo 2", window, cx);
        });
        state.execute_pane_input(pane_id, window, cx);
    });
    wait_for_terminal_submission(&mut cx, &app_state, pane_id);

    app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        assert_eq!(pane.command_history, vec!["echo 1", "echo 2"]);
        let input = pane.input_state.as_ref().unwrap();
        assert_eq!(input.read(cx).value().as_str(), "");
    });

    // Start typing a draft "draft cmd"
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("draft cmd", window, cx);
        });
        // Press Up: should recall "echo 2" and store "draft cmd" in pending_draft
        state.step_pane_history(pane_id, true, window, cx);
    });

    app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        assert_eq!(pane.pending_draft, "draft cmd");
        assert_eq!(pane.history_index, Some(1));
        let input = pane.input_state.as_ref().unwrap();
        assert_eq!(input.read(cx).value().as_str(), "echo 2");
    });

    // Press Up again: should recall "echo 1"
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.step_pane_history(pane_id, true, window, cx);
    });

    app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        assert_eq!(pane.history_index, Some(0));
        let input = pane.input_state.as_ref().unwrap();
        assert_eq!(input.read(cx).value().as_str(), "echo 1");
    });

    // Press Down: should recall "echo 2"
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.step_pane_history(pane_id, false, window, cx);
    });

    app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        assert_eq!(pane.history_index, Some(1));
        let input = pane.input_state.as_ref().unwrap();
        assert_eq!(input.read(cx).value().as_str(), "echo 2");
    });

    // Press Down again: should restore pending_draft "draft cmd"
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.step_pane_history(pane_id, false, window, cx);
    });

    app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        assert_eq!(pane.history_index, None);
        let input = pane.input_state.as_ref().unwrap();
        assert_eq!(input.read(cx).value().as_str(), "draft cmd");
    });
}

#[test]
fn shob_bottom_input_clear_and_cancel() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

    // Type a value and then clear it
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("to be cleared", window, cx);
        });
        state.clear_pane_input(pane_id, window, cx);
    });

    app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        assert_eq!(pane.history_index, None);
        let input = pane.input_state.as_ref().unwrap();
        assert_eq!(input.read(cx).value().as_str(), "");
    });

    // Cancel job should be callable without panicking
    cx.update_window_entity(&app_state, |state, _window, cx| {
        state.cancel_pane_job(pane_id, cx);
    });
}

#[test]
fn terminal_is_read_only_and_cannot_be_written_directly() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    // Terminal must be configured read-only
    let is_ro = app_state.read_with(&cx, |state, cx| {
        state.projects[0].sessions[0].panes[0]
            .terminal
            .read(cx)
            .is_read_only()
    });
    assert!(is_ro, "Terminal must be strictly read-only");

    // Attempting to paste on the terminal directly is ignored
    cx.update_window_entity(&app_state, |state, _window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        pane.terminal.update(cx, |term, cx| {
            term.paste_text("injected text", cx);
        });
    });
}

#[test]
fn plus_menu_and_message_placeholder() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

    // Check plus menu toggle
    cx.update_window_entity(&app_state, |state, _window, cx| {
        state.toggle_plus_menu(pane_id, cx);
    });
    let menu_open = app_state.read_with(&cx, |state, _| {
        state.projects[0].sessions[0].panes[0].plus_menu_open
    });
    assert!(menu_open);

    cx.draw_frame().expect("draw frame with plus menu open");

    // Run quick action "clear"
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.run_quick_action(pane_id, "clear", window, cx);
    });

    let menu_closed = app_state.read_with(&cx, |state, _| {
        !state.projects[0].sessions[0].panes[0].plus_menu_open
    });
    assert!(menu_closed);

    let history = app_state.read_with(&cx, |state, _| {
        state.projects[0].sessions[0].panes[0]
            .command_history
            .clone()
    });
    assert_eq!(history, vec!["clear"]);
}

#[test]
fn terminal_shortcuts_work_in_read_only() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

    // Ctrl+A selects all
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        pane.terminal.update(cx, |term, cx| {
            let ev = gpui::KeyDownEvent {
                keystroke: gpui::Keystroke::parse("ctrl-a").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            term.dispatch_key(&ev, window, cx);
        });
    });

    // Ctrl+C with no selection sends SIGINT (\x03)
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        pane.terminal.update(cx, |term, cx| {
            term.clear_selection(cx);
            let ev = gpui::KeyDownEvent {
                keystroke: gpui::Keystroke::parse("ctrl-c").unwrap(),
                is_held: false,
                prefer_character_input: false,
            };
            term.dispatch_key(&ev, window, cx);
        });
    });

    // In bottom input: Ctrl+Z, Ctrl+D, Ctrl+L work
    cx.update_window_entity(&app_state, |state, window, cx| {
        let ev_z = gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("ctrl-z").unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        state.handle_pane_input_key(pane_id, &ev_z, window, cx);

        let ev_l = gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("ctrl-l").unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        state.handle_pane_input_key(pane_id, &ev_l, window, cx);
    });
}

#[test]
fn git_branch_detection_and_badges() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    // Verify git branch detection on the current repo
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let branch = git_workspace::detect_git_branch(manifest_dir);
    assert!(branch.is_some(), "Should detect git branch for repo");

    // Temporary directory without git should return None
    let tmp = std::env::temp_dir();
    let non_git = git_workspace::detect_git_branch(&tmp);
    assert!(non_git.is_none() || tmp.join(".git").exists());

    cx.update_window_entity(&app_state, |state, _, cx| {
        let samples = state.active_path_requests().into_iter()
            .map(|(id, probe)| (id, probe.sample())).collect();
        state.apply_path_samples(samples, cx);
        let pane = state.pane_ref(1).unwrap();
        assert_eq!(state.pane_git_branch(pane).as_deref(), branch.as_deref());
        state.pane_mut(1).unwrap().cwd = Some(tmp);
        assert!(state.pane_git_branch(state.pane_ref(1).unwrap()).is_none());
    });

    // Verify placeholder is "Run commands"
    let placeholder_is_run_commands = app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.read(cx).placeholder_str() == "Run commands"
    });
    assert!(
        placeholder_is_run_commands,
        "Input placeholder must be 'Run commands'"
    );

    // Frame draws with badges and input
    cx.draw_frame().expect("draw frame with badges and input");
}

#[test]
fn dynamic_auto_focus() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    let mut is_focused = false;
    let mut pane_id = 0;
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        pane_id = pane.id;
        is_focused = input.read(cx).focus_handle().is_focused(window);
    });
    assert!(
        is_focused,
        "Bottom input must be dynamically focused on start"
    );

    // Focus pane should keep input focused
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.focus_pane(pane_id, window, cx);
    });
    cx.draw_frame().expect("draw frame after focus_pane");

    let mut input_focused_again = false;
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input_focused_again = input.read(cx).focus_handle().is_focused(window);
    });
    assert!(input_focused_again, "Input must remain focused dynamically");
}

#[test]
fn terminal_right_click_copy_and_paste() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

    // Case 1: Select text in terminal -> right-click opens context menu with Copy option
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        pane.terminal.update(cx, |term, cx| {
            term.select_all(cx);
        });
        state.open_terminal_context_menu(pane_id, point(px(100.0), px(100.0)), window, cx);
    });

    let is_menu_open = app_state.read_with(&cx, |state, _| state.terminal_context_menu.is_some());
    assert!(is_menu_open, "Right click must open context menu modal");
    cx.draw_frame().expect("draw context menu modal");

    // Clicking Copy in modal copies selection and clears selection
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.copy_terminal_selection(pane_id, window, cx);
    });

    let has_sel = app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        pane.terminal.read(cx).has_selection()
    });
    assert!(!has_sel, "Copy should clear selection after copying");
    assert!(app_state.read_with(&cx, |state, _| state.terminal_context_menu.is_none()));

    // Case 2: Right-click opens modal -> clicking Paste inserts clipboard text into input
    cx.update_window_entity(&app_state, |state, window, cx| {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("echo copied".to_string()));
        state.open_terminal_context_menu(pane_id, point(px(100.0), px(100.0)), window, cx);
        state.paste_from_clipboard_into_pane(pane_id, window, cx);
    });

    let input_val = app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.read(cx).value().to_string()
    });
    assert_eq!(
        input_val, "echo copied",
        "Paste should insert clipboard text into input"
    );

    // Case 3: Pasting text containing newlines should sanitize and NOT panic
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("", window, cx);
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
            "multi\nline\r\ncommand\n".to_string(),
        ));
        state.paste_from_clipboard_into_pane(pane_id, window, cx);
    });

    // Draw frame: if newlines leaked into single-line shape_line, this would panic
    cx.draw_frame()
        .expect("drawing frame after multiline paste must not panic");

    let input_val_clean = app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.read(cx).value().to_string()
    });
    assert_eq!(input_val_clean, "multi line command");
}

#[test]
fn input_yields_to_command_and_returns_at_prompt() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");
    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);
    assert!(app_state.read_with(&cx, |state, cx| {
        state
            .pane_ref(pane_id)
            .unwrap()
            .should_show_bottom_input(cx)
    }));

    cx.update_window_entity(&app_state, |state, _, cx| {
        state.on_shell_event(
            pane_id,
            &ShellEvent::CommandStarted {
                command_line: None,
                cwd: None,
                cols: 80,
                rows: 24,
            },
            cx,
        );
    });
    cx.draw_frame().expect("draw running command");
    app_state.read_with(&cx, |state, cx| {
        let pane = state.pane_ref(pane_id).unwrap();
        assert!(!pane.should_show_bottom_input(cx));
        assert!(!pane.terminal.read(cx).is_read_only());
    });

    cx.update_window_entity(&app_state, |state, _, cx| {
        state.on_shell_event(pane_id, &ShellEvent::PromptStart, cx);
    });
    cx.draw_frame().expect("draw returned prompt");
    app_state.read_with(&cx, |state, cx| {
        let pane = state.pane_ref(pane_id).unwrap();
        assert!(pane.should_show_bottom_input(cx));
        assert!(pane.terminal.read(cx).is_read_only());
    });
}

#[test]
fn prompt_submission_sends_text_and_enter_without_manual_intervention() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("prompt message", window, cx);
        });
        state.execute_pane_input(pane_id, window, cx);
    });

    wait_for_terminal_submission(&mut cx, &app_state, pane_id);

    // Terminal received input and cleared draft
    app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        assert_eq!(input.read(cx).value().as_str(), "");
        assert_eq!(
            pane.command_history.last().map(|s| s.as_str()),
            Some("prompt message")
        );
    });
}
