#[test]
fn saving_a_workspace_records_each_panes_own_model() {
    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    let deepseek = agent_auth::AgentModel {
        provider: "commandcode".into(),
        id: "deepseek/deepseek-v4-flash".into(),
        backend: agent_auth::ModelBackend::Shob,
    };
    let grok = agent_auth::AgentModel {
        provider: "xai-oauth".into(),
        id: "grok-4.7".into(),
        backend: agent_auth::ModelBackend::Shob,
    };
    cx.update_window_entity(&state, |s, window, cx| {
        s.split_pane(1, gpui::Axis::Horizontal, window, cx);
        // Set the models directly so no agent process is spawned.
        let panes = &mut s.projects[0].sessions[0].panes;
        panes[0].chat.selected_model = Some(deepseek.clone());
        panes[0].chat.model_explicit = true;
        panes[1].chat.selected_model = Some(grok.clone());
        panes[1].chat.model_explicit = true;
        s.schedule_workspace_save(cx);
    });
    let saved = state.read_with(&cx, |s, cx| s.saved_workspace(cx));
    let models: Vec<_> = saved.projects[0].sessions[0]
        .panes
        .iter()
        .map(|pane| pane.chat.selected_model.clone())
        .collect();
    assert_eq!(models, vec![Some(deepseek), Some(grok)]);
}

#[test]
fn an_empty_project_remains_selected_after_workspace_restore() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().to_path_buf();
    let workspace = workspace_state::SavedWorkspace {
        active_project: Some(path.clone()),
        projects: vec![workspace_state::SavedProject {
            id: 7,
            name: "Empty project".into(),
            path: path.clone(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |state, _, cx| {
        state.startup_loaded = false;
        state.apply_saved_workspace(
            RestoreData {
                workspace,
                scrollbacks: HashMap::new(),
                transcripts: HashMap::new(),
                usage: HashMap::new(),
            },
            cx,
        );
    });
    cx.draw_frame().unwrap();
    state.read_with(&cx, |state, _| {
        assert_eq!(state.active_project().unwrap().path, path);
        assert!(state.active_session().is_none());
        assert!(state.focused_pane_id.is_none());
    });
}

#[test]
fn restored_panes_use_the_global_model() {
    // The reported bug: pick deepseek in pane A and grok in pane B, restart,
    // and both came back pinned to the app default instead of their own.
    let mut workspace = workspace_state::SavedWorkspace {
        version: workspace_state::CURRENT_VERSION,
        active_project: Some(std::env::current_dir().unwrap()),
        projects: vec![workspace_state::SavedProject {
            id: 1,
            path: std::env::current_dir().unwrap(),
            name: "shob".into(),
            projectless: false,
            expanded: true,
            active_session: 0,
            sessions: vec![workspace_state::SavedSession {
                id: 2,
                uid: "s-1".into(),
                title: "agent".into(),
                display_title: "agent".into(),
                logo: String::new(),
                maximized_pane_id: None,
                focused_pane: Some(4),
                tree: workspace_state::SavedPaneNode::Split {
                    id: 6,
                    axis: workspace_state::SavedAxis::Horizontal,
                    left: Box::new(workspace_state::SavedPaneNode::Leaf { pane: 4 }),
                    right: Box::new(workspace_state::SavedPaneNode::Leaf { pane: 5 }),
                },
                panes: vec![
                    workspace_state::SavedPane {
                        id: 4,
                        uid: "p-1".into(),
                        title: "a".into(),
                        display_title: "a".into(),
                        chat_active: true,
                        chat: workspace_state::SavedChat {
                            selected_model: Some(agent_auth::AgentModel {
                                provider: "commandcode".into(),
                                id: "deepseek/deepseek-v4-flash".into(),
                                backend: agent_auth::ModelBackend::Shob,
                            }),
                            model_explicit: true,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    workspace_state::SavedPane {
                        id: 5,
                        uid: "p-2".into(),
                        title: "b".into(),
                        display_title: "b".into(),
                        chat_active: true,
                        chat: workspace_state::SavedChat {
                            selected_model: Some(agent_auth::AgentModel {
                                provider: "xai-oauth".into(),
                                id: "grok-4.7".into(),
                                backend: agent_auth::ModelBackend::Shob,
                            }),
                            model_explicit: true,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                ],
            }],
        }],
        threads: Vec::new(),
        bots: Vec::new(),
    };
    let saved_chat = &mut workspace.projects[0].sessions[0].panes[0].chat;
    let legacy_key = conversations::legacy_conversation_key(saved_chat.selected_model.as_ref());
    saved_chat.models.push(workspace_state::SavedModelChat {
        model: saved_chat.selected_model.clone(),
        conversation: legacy_key.clone(),
        agent_title: Some("Existing chat".into()),
        ..Default::default()
    });
    let transcripts = HashMap::from([(
        transcript_key("p-1", &legacy_key),
        conversations::RestoredConversation {
            messages: vec![Arc::new(ChatMessage::new(
                ChatRole::User,
                "Legacy history".into(),
            ))],
            ..Default::default()
        },
    )]);
    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    let default = agent_auth::AgentModel {
        provider: "google-antigravity".into(),
        id: "gemini-3.8-flash".into(),
        backend: agent_auth::ModelBackend::Shob,
    };
    cx.update_window_entity(&state, |s, _, cx| {
        s.settings.default_agent_model = Some(default.clone());
        s.settings.permission_mode = agent_bridge::PermissionMode::Auto;
        s.startup_loaded = false;
        s.dirty = false;
        s.schedule_workspace_save(cx);
        s.save_all_sync(None, cx);
        assert!(!s.startup_loaded);
        assert!(!s.dirty);
        s.settings.restore.agent_history = true;
        s.apply_saved_workspace(
            RestoreData {
                workspace: workspace.clone(),
                scrollbacks: HashMap::new(),
                transcripts,
                usage: HashMap::new(),
            },
            cx,
        );
    });
    state.read_with(&cx, |s, cx| {
        let pane = &s.projects[0].sessions[0].panes[0];
        assert_eq!(
            pane.chat.permission_mode,
            agent_bridge::PermissionMode::Auto
        );
        assert_eq!(pane.chat.conversation_key, legacy_key);
        assert_eq!(pane.chat.conversation_title(), Some("Existing chat"));
        assert_eq!(pane.chat.visible_messages()[0].text, "Legacy history");
        assert_eq!(
            s.saved_workspace(cx).projects[0].sessions[0].panes[0]
                .chat
                .conversation_key
                .as_deref(),
            Some(legacy_key.as_str()),
        );
        let models: Vec<_> = s.projects[0].sessions[0]
            .panes
            .iter()
            .map(|pane| pane.chat.selected_model.clone())
            .collect();
        assert!(models.iter().all(|model| model.as_ref() == Some(&default)));
    });
}

#[test]
fn restored_pane_adopts_the_global_model() {
    let workspace = workspace_state::SavedWorkspace {
        version: workspace_state::CURRENT_VERSION,
        active_project: Some(std::env::current_dir().unwrap()),
        projects: vec![workspace_state::SavedProject {
            id: 1,
            path: std::env::current_dir().unwrap(),
            name: "shob".into(),
            projectless: false,
            expanded: true,
            active_session: 0,
            sessions: vec![workspace_state::SavedSession {
                id: 2,
                uid: "s-1".into(),
                title: "agent".into(),
                display_title: "agent".into(),
                logo: String::new(),
                maximized_pane_id: None,
                focused_pane: Some(4),
                tree: workspace_state::SavedPaneNode::Leaf { pane: 4 },
                panes: vec![workspace_state::SavedPane {
                    id: 4,
                    uid: "p-1".into(),
                    title: "a".into(),
                    display_title: "a".into(),
                    chat_active: true,
                    chat: workspace_state::SavedChat {
                        model_explicit: true,
                        ..Default::default()
                    },
                    ..Default::default()
                }],
            }],
        }],
        threads: Vec::new(),
        bots: Vec::new(),
    };
    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    cx.update_window_entity(&state, |s, _, cx| {
        s.settings.default_agent_model = Some(agent_auth::AgentModel {
            provider: "xai-oauth".into(),
            id: "grok-4.7".into(),
            backend: agent_auth::ModelBackend::Shob,
        });
        s.startup_loaded = false;
        s.apply_saved_workspace(
            RestoreData {
                workspace,
                scrollbacks: HashMap::new(),
                transcripts: HashMap::new(),
                usage: HashMap::new(),
            },
            cx,
        );
    });
    state.read_with(&cx, |s, _| {
        let pane = &s.projects[0].sessions[0].panes[0];
        assert_eq!(
            pane.chat.selected_model.as_ref(),
            s.settings.default_agent_model.as_ref()
        );
        assert!(!pane.chat.model_explicit);
    });
}

#[test]
fn model_selector_always_names_the_provider() {
    // `commandcode` model ids carry their own slash; prefixing the provider
    // is what stops the backend from guessing a different one.
    let model = agent_auth::AgentModel {
        provider: "commandcode".into(),
        id: "deepseek/deepseek-v4-flash".into(),
        backend: agent_auth::ModelBackend::Shob,
    };
    assert_eq!(
        agent_model_selector(&model),
        "commandcode/deepseek/deepseek-v4-flash"
    );
}

#[test]
fn window_resizes_without_panicking() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    let terminal = app_state.read_with(&cx, |state, _| {
        state.active_session().expect("a session").panes[0]
            .terminal
            .clone()
    });

    let mut grid_sizes = Vec::new();
    for window_size in [
        size(px(1100.), px(700.)),
        size(px(900.), px(600.)),
        size(px(700.), px(500.)),
        size(px(520.), px(400.)),
        size(px(380.), px(300.)),
        size(px(280.), px(220.)),
        size(px(200.), px(160.)),
        size(px(1200.), px(780.)),
    ] {
        cx.simulate_resize(window_size);
        cx.run_until_parked();
        cx.draw_frame().expect("a frame at the new size");

        // The grid has to track the window, and every scrape has to come
        // back with exactly the rows the grid has: a scrape that names rows
        // the grid resized away is what aborted the process on a drag.
        let (cols, rows) = terminal.read_with(&cx, |view, _| view.dimensions());
        assert!(cols >= 2 && rows >= 1, "grid collapsed to {cols}x{rows}");
        let scraped = terminal.read_with(&cx, |view, _| view.bottom_lines(usize::MAX));
        assert_eq!(scraped.len(), rows, "scrape out of step with the grid");
        grid_sizes.push((cols, rows));
    }

    // ...and the grid really did follow the window, rather than both sides
    // agreeing on a size nobody updated.
    let (big_cols, big_rows) = grid_sizes[0];
    let (small_cols, small_rows) = grid_sizes[6];
    assert!(
        small_rows < big_rows && small_cols < big_cols,
        "grid did not follow the window: {small_cols}x{small_rows} at 200x160, \
             {big_cols}x{big_rows} at 1100x700"
    );
}

/// The status poller runs between frames, so it can tick after the paint
/// pass has already shrunk the terminal grid but before the next render
/// syncs the cached row count. Scraping the screen in that window asks the
/// grid for rows that no longer exist.
#[test]
fn status_poll_between_resize_frames_does_not_panic() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);

    // A frame at the large size: the grid is laid out big while the cached
    // row count still holds the size from the previous frame.
    cx.draw_frame().expect("a frame at the opening size");

    // Shrink the window and draw one frame: the render syncs the cached
    // dims to the large grid, then the paint shrinks the grid.
    cx.simulate_resize(size(px(560.), px(320.)));
    cx.run_until_parked();
    cx.draw_frame().expect("a frame at the smaller size");

    // The poller's ticks are what a user sees as a crash while dragging.
    app_state.update(&mut cx, |state, cx| state.poll_sessions_status(cx));
    app_state.update(&mut cx, |state, cx| state.poll_sessions_status(cx));
}

/// Simulating dragging across the fixed sidebar and window bounds must
/// stay panic-free.
#[test]
fn dragging_the_sidebar_handle_does_not_panic() {
    let mut cx = TestAppContext::new();
    let (mut cx, _) = open_app(&mut cx);
    cx.draw_frame().expect("a frame at the opening size");

    let y = px(400.0);
    let grab = point(px(327.5), y);
    cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::none());
    for dx in [-400.0, -50.0, 0.0, 60.0, 200.0, 400.0, -800.0, 30.0] {
        cx.simulate_mouse_move(
            point(px(327.5 + dx), y),
            Some(MouseButton::Left),
            Modifiers::none(),
        );
        cx.draw_frame().expect("frame while dragging sidebar");
    }
    cx.simulate_mouse_up(grab, MouseButton::Left, Modifiers::none());
    cx.draw_frame().expect("frame after dragging sidebar");
}

#[test]
fn sidebar_toggle_and_resize_without_panicking() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);

    for visible in [false, true, false, true] {
        app_state.update(&mut cx, |state, cx| {
            state.sidebar_visible = visible;
            cx.notify();
        });

        cx.simulate_resize(size(px(640.), px(480.)));
        cx.run_until_parked();
        cx.draw_frame().expect("a frame after toggling");
        cx.simulate_resize(size(px(1200.), px(780.)));
        cx.run_until_parked();
        cx.draw_frame().expect("a frame after toggling");
    }
}

#[test]
fn compact_sidebar_opens_and_dismisses_without_changing_preference() {
    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    cx.simulate_resize(size(px(640.0), px(480.0)));
    cx.run_until_parked();
    cx.draw_frame().expect("compact frame");
    let toggle = cx.debug_bounds("terminal-sidebar-toggle-btn").unwrap();
    cx.simulate_click(toggle.center(), Modifiers::none());
    cx.draw_frame().expect("open drawer");
    assert!(state.read_with(&cx, |state, _| state.compact_sidebar_open));
    cx.simulate_mouse_down(
        point(px(634.0), px(200.0)),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.draw_frame().expect("dismissed drawer");
    assert!(state.read_with(&cx, |state, _| !state.compact_sidebar_open
        && state.sidebar_visible));
}

#[test]
fn compact_sidebar_keeps_saved_preference_separate() {
    assert!(!AppState::sidebar_docked(true, px(767.0)));
    assert!(AppState::sidebar_docked(true, px(768.0)));
    assert!(!AppState::sidebar_docked(false, px(1200.0)));
}

#[test]
fn fixed_sidebar_width_stays_constant_on_resize() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);

    for new_size in [
        size(px(640.), px(480.)),
        size(px(1440.), px(900.)),
        size(px(1920.), px(1080.)),
    ] {
        cx.simulate_resize(new_size);
        cx.run_until_parked();
        cx.draw_frame().expect("drawn frame");

        let width = app_state.read_with(&cx, |state, _| state.sidebar_width);
        assert_eq!(
            width, SIDEBAR_WIDTH,
            "sidebar width must remain fixed across window resizes"
        );
    }
}
