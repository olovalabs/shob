#[test]
fn running_sidebar_card_disappears_when_session_finishes() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("initial frame");
    assert!(cx.debug_bounds("sidebar-running-sessions").is_none());

    cx.update_window_entity(&app_state, |state, _, cx| {
        let project = &mut state.projects[0];
        project.expanded = false;
        let session = &mut project.sessions[0];
        session.panes[0].status = SessionStatus::Working;
        cx.notify();
    });
    cx.draw_frame().expect("running frame");
    assert!(cx.debug_bounds("sidebar-running-sessions").is_some());

    cx.update_window_entity(&app_state, |state, _, cx| {
        state.projects[0].sessions[0].panes[0].status = SessionStatus::Done;
        cx.notify();
    });
    cx.draw_frame().expect("completed frame");
    assert!(cx.debug_bounds("sidebar-running-sessions").is_none());
}

#[test]
fn sidebar_renders_collapsed_and_expanded_projects() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("initial frame");

    let dir = scratch_project_dir("shob-multi-project-collapse");
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.open_project_path(dir, window, cx);
    });
    cx.draw_frame().expect("frame with two expanded projects");

    // Collapse the first project while the second stays expanded.
    cx.update_window_entity(&app_state, |state, _window, cx| {
        state.toggle_project_expanded(0, cx);
    });
    cx.draw_frame().expect("frame with one collapsed project");
    let expanded = app_state.read_with(&cx, |state, _| {
        (state.projects[0].expanded, state.projects[1].expanded)
    });
    assert_eq!(expanded, (false, true));

    // Activating a collapsed project expands it again.
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.activate_project(0, window, cx);
    });
    cx.draw_frame().expect("frame after activating a project");
    let (active, expanded) = app_state.read_with(&cx, |state, _| {
        (state.active_project_idx, state.projects[0].expanded)
    });
    assert_eq!(active, 0);
    assert!(expanded);
}

#[test]
fn sidebar_renders_with_agent_and_worker() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);

    cx.update_window_entity(&app_state, |state, _window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        pane.is_agent = true;
        pane.app = Some(SharedString::from("claude"));
        pane.status = SessionStatus::Done;
        pane.attention.seen = false;
        pane.attention.last = Some(SessionStatus::Done);
        pane.attention.since = 42;
        pane.sub_agents.insert(
            "child".into(),
            attention::SubAgent {
                id: "child".into(),
                title: Some("explore".into()),
                status: SessionStatus::Working,
                since: 41,
                seq: 41,
            },
        );
        cx.notify();
    });
    // Drawing a frame with agent and sub-agent panes succeeds without panic.
    cx.draw_frame().expect("frame with agent pane");
    let agents = app_state.read_with(&cx, |state, _| {
        state
            .projects
            .iter()
            .flat_map(|project| &project.sessions)
            .flat_map(|session| &session.panes)
            .filter(|pane| pane.is_agent)
            .count()
    });
    assert_eq!(agents, 1);
}

#[test]
fn sidebar_new_session_and_remove_session_icons_align() {
    let mut cx = TestAppContext::new();
    let (mut cx, _) = open_app(&mut cx);
    cx.draw_frame()
        .expect("initial frame with project and session");

    let new_session_bounds = cx
        .debug_bounds("project-new-session")
        .expect("project-new-session button bounds");
    let remove_session_bounds = cx
        .debug_bounds("session-close")
        .expect("session-close button bounds");
    let new_session_right = new_session_bounds.origin.x + new_session_bounds.size.width;
    let remove_session_right = remove_session_bounds.origin.x + remove_session_bounds.size.width;
    assert_eq!(
        new_session_right, remove_session_right,
        "create new session icon and remove session icon must have the same right alignment"
    );
    assert_eq!(
        new_session_bounds.origin.x, remove_session_bounds.origin.x,
        "create new session icon and remove session icon must have the same horizontal start"
    );
    assert_eq!(
        new_session_bounds.size.width, remove_session_bounds.size.width,
        "create new session icon and remove session icon must have the same width"
    );
    assert_eq!(new_session_bounds.size.width, px(20.0));
    assert_eq!(new_session_right, px(249.0));
    assert_eq!(remove_session_right, px(249.0));
    assert_eq!(new_session_bounds.origin.x, px(229.0));
    assert_eq!(remove_session_bounds.origin.x, px(229.0));
}

#[test]
fn sidebar_session_indices_keep_the_newest_on_the_first_page() {
    assert_eq!(
        sidebar_session_indices(0, 5).collect::<Vec<_>>(),
        Vec::<usize>::new()
    );
    assert_eq!(
        sidebar_session_indices(3, 5).collect::<Vec<_>>(),
        vec![2, 1, 0]
    );
    assert_eq!(
        sidebar_session_indices(6, 5).collect::<Vec<_>>(),
        vec![5, 4, 3, 2, 1]
    );
    assert_eq!(
        sidebar_session_indices(17, 15).collect::<Vec<_>>(),
        (2..17).rev().collect::<Vec<_>>()
    );
}

#[test]
fn sidebar_keeps_an_older_active_session_visible_without_duplicates() {
    assert_eq!(
        sidebar_visible_sessions(8, 5, Some(0)),
        vec![7, 6, 5, 4, 3, 0]
    );
    assert_eq!(sidebar_visible_sessions(8, 5, Some(5)), vec![7, 6, 5, 4, 3]);
    assert_eq!(sidebar_visible_sessions(2, 5, Some(9)), vec![1, 0]);
    assert!(sidebar_visible_sessions(0, 5, Some(0)).is_empty());
}

#[test]
fn new_sidebar_session_renders_first_and_stays_on_the_first_page() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.update_window_entity(&app_state, |state, _, cx| {
        state.settings.agent_layout = true;
        for idx in 2..=6 {
            let title: SharedString = format!("Session {idx}").into();
            state.projects[0].sessions.push(Session {
                id: idx,
                uid: paths::uid("s"),
                title: title.clone(),
                display_title: title,
                logo: logos::default_logo(),
                pane_tree: None,
                maximized_pane_id: None,
                panes: Vec::new(),
                status: SessionStatus::Idle,
            });
        }
        state.next_id = 7;
        state.rebuild_session_index();
        cx.notify();
    });
    cx.draw_frame().expect("sidebar with New chat");
    let new_thread = cx.debug_bounds("sidebar-new-thread").unwrap();
    cx.simulate_click(new_thread.center(), Modifiers::default());
    app_state.read_with(&cx, |state, _| {
        let chat = state.active_session().unwrap();
        assert!(chat.panes.iter().any(|pane| pane.chat.active));
        assert_eq!(chat.id, 7);
    });
    cx.draw_frame().expect("first fresh chat");
    let new_thread = cx.debug_bounds("sidebar-new-thread").unwrap();
    cx.simulate_click(new_thread.center(), Modifiers::default());
    app_state.read_with(&cx, |state, _| {
        assert_eq!(state.active_session().unwrap().id, 7);
        assert_eq!(state.active_project().unwrap().sessions.len(), 7);
    });
    let project_plus = cx.debug_bounds("project-new-session").unwrap().center();
    cx.simulate_mouse_move(project_plus, None, Modifiers::default());
    cx.draw_frame().unwrap();
    cx.simulate_click(project_plus, Modifiers::default());
    app_state.read_with(&cx, |state, _| {
        assert_eq!(state.active_session().unwrap().id, 7);
        assert_eq!(state.active_project().unwrap().sessions.len(), 7);
    });
    cx.update_window_entity(&app_state, |state, _, cx| {
        let pane_id = state.focused_pane_id.unwrap();
        state
            .pane_mut(pane_id)
            .unwrap()
            .chat
            .messages
            .push(Arc::new(ChatMessage::new(ChatRole::User, "Hello".into())));
        cx.notify();
    });
    cx.draw_frame().expect("chat with a message");
    let new_thread = cx.debug_bounds("sidebar-new-thread").unwrap();
    cx.simulate_click(new_thread.center(), Modifiers::default());
    app_state.read_with(&cx, |state, _| {
        let chat = state.active_session().unwrap();
        assert_eq!(chat.id, 10);
        assert!(chat.panes.iter().any(|pane| pane.chat.active));
    });
    cx.draw_frame()
        .expect("sidebar with the newest session first");

    let newest = cx
        .debug_bounds("session-row-10")
        .expect("newest session row");
    let previous = cx
        .debug_bounds("session-row-7")
        .expect("previous session row");
    assert!(
        newest.top() < previous.top(),
        "a new session must render above older sessions"
    );
    assert!(
        cx.debug_bounds("session-row-1").is_none(),
        "the oldest session must leave the first page instead of hiding the new one"
    );
}

#[test]
fn sidebar_search_opens_a_match_from_a_collapsed_project() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.update_window_entity(&app_state, |state, _, cx| {
        let title = SharedString::from(format!("{} Release Notes", "Long title ".repeat(16)));
        state.projects.push(Project {
            id: 2,
            name: "Other project".into(),
            path: state.projects[0].path.clone(),
            sessions: vec![Session {
                id: 3,
                uid: paths::uid("s"),
                display_title: truncate_title(&title),
                title,
                logo: logos::default_logo(),
                pane_tree: None,
                maximized_pane_id: None,
                panes: Vec::new(),
                status: SessionStatus::Idle,
            }],
            active_session_idx: 0,
            expanded: false,
            visible_session_count: 5,
        });
        state.rebuild_session_index();
        cx.notify();
    });
    cx.draw_frame().unwrap();
    let search = cx.debug_bounds("sidebar-search").unwrap();
    assert!(cx.debug_bounds("session-row-3").is_none());
    cx.simulate_click(search.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    assert_eq!(
        cx.debug_bounds("sidebar-search-field").unwrap().top(),
        search.top()
    );

    cx.simulate_input("NOTESz");
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("sidebar-search-empty").is_some());
    cx.simulate_keystrokes("backspace");
    cx.draw_frame().unwrap();
    let result = cx.debug_bounds("session-row-3").expect("full title match");
    assert!(cx.debug_bounds("session-row-1").is_none());
    cx.simulate_click(result.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("sidebar-search-field").is_none());
    app_state.read_with(&cx, |state, _| {
        assert_eq!(state.active_session().unwrap().id, 3);
    });
}

#[test]
fn project_rename_flow() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("initial frame");

    let project_id = app_state.read_with(&cx, |state, _| state.projects[0].id);

    // Start rename
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.start_renaming_project(project_id, window, cx);
    });
    assert!(app_state.read_with(&cx, |state, _| state.renaming_project.is_some()));

    // Frame should render the inline input cleanly
    cx.draw_frame().expect("frame while renaming project");

    // Finish rename
    cx.update_window_entity(&app_state, |state, _, cx| {
        state.finish_renaming_project(project_id, SharedString::from("renamed-project"), cx);
    });

    let (name, is_renaming) = app_state.read_with(&cx, |state, _| {
        (
            state.projects[0].name.clone(),
            state.renaming_project.is_some(),
        )
    });
    assert_eq!(name, "renamed-project");
    assert!(!is_renaming);

    // Frame renders with new project name
    cx.draw_frame().expect("frame with renamed project");
}

#[test]
fn project_menu_and_remove_flow() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);

    let dir = scratch_project_dir("shob-menu-remove");
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.open_project_path(dir, window, cx);
    });
    cx.draw_frame().expect("frame with two projects");

    let (count, project_id) =
        app_state.read_with(&cx, |state, _| (state.projects.len(), state.projects[1].id));
    assert_eq!(count, 2);

    // Open 3-dot dropdown menu
    cx.update_window_entity(&app_state, |state, _, cx| {
        state.project_menu = Some(project_id);
        cx.notify();
    });
    // Frame renders menu popover and backdrop
    cx.draw_frame().expect("frame with project menu open");

    // Close/Remove the project
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.close_project(1, window, cx);
    });
    cx.draw_frame().expect("frame after removing project");

    let (final_count, menu) =
        app_state.read_with(&cx, |state, _| (state.projects.len(), state.project_menu));
    assert_eq!(final_count, 1);
    assert_eq!(menu, None);
}
