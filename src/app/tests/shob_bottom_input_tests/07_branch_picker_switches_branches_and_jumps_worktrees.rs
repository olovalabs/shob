#[test]
fn branch_picker_switches_branches_and_jumps_worktrees() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("branch-repo");
    let wt_root = temp.path().join("worktrees");
    init_test_git_repo(&repo, "main");
    test_git(&repo, &["branch", "feat/test"]);
    let wt = git_workspace::create_worktree(&repo, &wt_root, Some("main")).unwrap();

    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);

    cx.update_window_entity(&state, |s, window, cx| {
        s.projects[0].path = repo.clone();
        s.projects[0].name = SharedString::from("branch-repo");
        s.set_chat_mode(pane_id, true, window, cx);
        s.settings.agent_layout = true;
        s.palette_scheme = PaletteScheme::MidnightGarden;
        s.theme_preference = ThemePreference::Dark;
        s.apply_theme_preference(Some(window), cx);
        if let Some(pane) = s.pane_mut(pane_id) {
            pane.pid = None;
            pane.cwd = Some(repo.clone());
        }
        s.schedule_git_refresh(repo.clone(), cx);
        cx.notify();
    });
    cx.run_until_parked();
    cx.draw_frame().unwrap();

    let branch_btn = cx.debug_bounds("agent-branch-selector").unwrap();
    cx.simulate_click(branch_btn.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-branch-menu").is_some());
    assert!(cx.debug_bounds("agent-branch-search").is_some());

    cx.simulate_input("feat/test");
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("branch-option-1").is_none());
    let feat_btn = cx.debug_bounds("branch-option-0").unwrap();
    cx.simulate_click(feat_btn.center(), Modifiers::none());
    cx.run_until_parked();
    cx.draw_frame().unwrap();
    assert_eq!(
        AppState::detect_git_branch(&repo).as_deref(),
        Some("feat/test")
    );

    // Open branch picker again and select the branch checked out in the worktree -> switches workspace!
    let branch_btn = cx.debug_bounds("agent-branch-selector").unwrap();
    cx.simulate_click(branch_btn.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    cx.update_window_entity(&state, |s, window, cx| {
        let pane = s.pane_ref(pane_id).unwrap();
        assert!(pane.branch_picker.is_some());
        pane.input_state
            .as_ref()
            .unwrap()
            .update(cx, |input, cx| input.focus(window, cx));
    });
    cx.draw_frame().unwrap();
    let search = cx.debug_bounds("agent-branch-search").unwrap();
    cx.simulate_click(
        point(search.left() + px(3.0), search.center().y),
        Modifiers::none(),
    );
    cx.draw_frame().unwrap();
    cx.simulate_input(wt.branch.as_deref().unwrap());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("branch-option-1").is_none());
    let search = cx.debug_bounds("agent-branch-search").unwrap();
    cx.simulate_click(
        point(search.left() + px(24.0), search.center().y),
        Modifiers::none(),
    );
    cx.simulate_input("x");
    state.read_with(&cx, |s, cx| {
        let picker = s.pane_ref(pane_id).unwrap().branch_picker.as_ref().unwrap();
        assert_eq!(
            picker.input.read(cx).value().as_str(),
            format!("x{}", wt.branch.as_deref().unwrap())
        );
    });
    cx.simulate_keystrokes("backspace");
    cx.draw_frame().unwrap();
    let wt_branch_btn = cx.debug_bounds("branch-option-0").unwrap();
    cx.simulate_click(wt_branch_btn.center(), Modifiers::none());
    cx.run_until_parked();
    cx.draw_frame().unwrap();

    state.read_with(&cx, |s, _| {
        let pane = s.pane_ref(pane_id).unwrap();
        assert_eq!(
            pane.chat
                .workspace_override
                .as_deref()
                .map(|p| git_workspace::paths_equal(p, &wt.path)),
            Some(true)
        );
    });
}

#[test]
fn t3_sidebar_thread_sections_prioritize_snooze_settle_and_archive() {
    let now = 1_000_000;
    assert_eq!(
        SidebarThreadState::default().section(now),
        Some(SidebarSection::Active)
    );
    assert_eq!(
        SidebarThreadState {
            pinned: true,
            ..Default::default()
        }
        .section(now),
        Some(SidebarSection::Pinned)
    );
    assert_eq!(
        SidebarThreadState {
            settled: true,
            ..Default::default()
        }
        .section(now),
        Some(SidebarSection::Settled)
    );
    assert_eq!(
        SidebarThreadState {
            snoozed_until_ms: Some(now + 1),
            settled: true,
            ..Default::default()
        }
        .section(now),
        Some(SidebarSection::Snoozed)
    );
    assert_eq!(
        SidebarThreadState {
            archived: true,
            pinned: true,
            ..Default::default()
        }
        .section(now),
        None
    );
    assert_eq!(sidebar_relative_time(now, now - 5 * 60_000), "5m");
    assert!(sidebar_matches("build", "Rebuild the parser"));
}

#[test]
fn system_monitor_button_toggles_live_panel() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.draw_frame().unwrap();
    let button = cx.debug_bounds("system-monitor").unwrap();
    cx.simulate_click(button.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(state.read_with(&cx, |state, _| state.system_monitor_open));
    assert!(cx.debug_bounds("system-monitor-panel").is_some());
    cx.simulate_click(button.center(), Modifiers::none());
    assert!(!state.read_with(&cx, |state, _| state.system_monitor_open));
}

#[test]
fn sidebar_quick_settle_moves_active_thread_to_settled() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.draw_frame().unwrap();
    let row = cx.debug_bounds("session-row-1").unwrap();
    cx.simulate_mouse_move(row.center(), None, Modifiers::none());
    cx.draw_frame().unwrap();
    let settle = cx.debug_bounds("sidebar-quick-settle").unwrap();
    cx.simulate_click(settle.center(), Modifiers::none());
    state.read_with(&cx, |state, _| {
        let uid = &state.projects[0].sessions[0].uid;
        assert!(state.sidebar_thread_state(uid).settled);
        assert!(state.settings.sidebar.settled_expanded);
        let groups = state.sidebar_thread_groups();
        assert!(groups.active.is_empty());
        assert_eq!(groups.settled.len(), 1);
    });
}

#[test]
fn t3_sidebar_working_section_counts_and_keeps_active_when_collapsed() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |state, _, cx| {
        state.settings.agent_layout = false;
        state.settings.sidebar.working_expanded = true;
        state.projects[0].sessions[0].status = SessionStatus::Working;
        let title: SharedString = "Other running task".into();
        state.projects[0].sessions.push(Session {
            id: 999,
            uid: "working-test".into(),
            title: title.clone(),
            display_title: title,
            logo: logos::default_logo(),
            pane_tree: None,
            maximized_pane_id: None,
            panes: Vec::new(),
            status: SessionStatus::Working,
        });
        let groups = state.sidebar_thread_groups();
        assert_eq!(groups.working_count, 2);
        assert_eq!(groups.working.len(), 2);
        assert!(groups.active.is_empty());
        cx.notify();
    });
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("session-row-999").is_some());
    let toggle = cx.debug_bounds("sidebar-working-toggle").unwrap();
    cx.simulate_click(toggle.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("session-row-999").is_none());
    assert!(cx.debug_bounds("session-row-1").is_some());
    assert!(!state.read_with(&cx, |state, _| state.settings.sidebar.working_expanded));
}

#[test]
fn t3_sidebar_new_button_chooses_project_before_creating_thread() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |state, _, cx| {
        let scope = state.projects[0].path.clone();
        state.settings.sidebar.project_scope = Some(scope);
        cx.notify();
    });
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("sidebar-new-thread").is_some());
    assert!(cx.debug_bounds("sidebar-search-field").is_some());
    assert!(cx.debug_bounds("sidebar-footer").is_some());
    let new_thread = cx.debug_bounds("sidebar-new-thread").unwrap();
    cx.simulate_click(new_thread.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("new-session-project-modal").is_some());
    assert_eq!(
        state.read_with(&cx, |state, _| state.projects[0].sessions.len()),
        1
    );
    cx.simulate_keystrokes("enter");
    assert_eq!(
        state.read_with(&cx, |state, _| state.projects[0].sessions.len()),
        2
    );
}

#[test]
fn t3_sidebar_always_reveals_the_active_route() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |state, _, cx| {
        let uid = state.projects[0].sessions[0].uid.clone();
        state.settings.sidebar.project_scope = Some(PathBuf::from("/not-this-project"));
        state.settings.sidebar.settled_expanded = false;
        state.sidebar_threads.insert(
            uid,
            SidebarThreadState {
                settled: true,
                archived: true,
                ..Default::default()
            },
        );
        cx.notify();
    });
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("session-row-1").is_some());
}
