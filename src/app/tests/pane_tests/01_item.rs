use super::*;
use gpui::{Modifiers, VisualContext, point};
use resize_tests::{DrawFrame, TestAppContext, open_app};
#[test]
fn pane_node_split_and_contains() {
    let mut tree = PaneNode::Leaf(1);
    assert!(tree.contains(1));
    assert!(!tree.contains(2));
    assert_eq!(tree.first_leaf(), 1);

    assert!(tree.split(1, 2, 10, gpui::Axis::Horizontal));
    assert!(tree.contains(1));
    assert!(tree.contains(2));
    assert!(!tree.contains(3));

    assert!(tree.split(2, 3, 11, gpui::Axis::Vertical));
    assert!(tree.contains(3));
    assert_eq!(tree.first_leaf(), 1);
}

#[test]
fn preview_can_be_placed_to_the_left_of_a_right_hand_source() {
    let mut tree = PaneNode::Leaf(1);
    tree.split(1, 2, 10, gpui::Axis::Horizontal);
    tree.split(2, 3, 11, gpui::Axis::Horizontal);
    tree.swap_split_children(2, 3);
    assert_eq!(
        tree,
        PaneNode::Split {
            id: 10,
            axis: gpui::Axis::Horizontal,
            left: Box::new(PaneNode::Leaf(1)),
            right: Box::new(PaneNode::Split {
                id: 11,
                axis: gpui::Axis::Horizontal,
                left: Box::new(PaneNode::Leaf(3)),
                right: Box::new(PaneNode::Leaf(2)),
            }),
        }
    );
}

#[test]
fn pane_node_remove_collapses_tree() {
    let mut tree = PaneNode::Leaf(1);
    tree.split(1, 2, 10, gpui::Axis::Horizontal);
    tree.split(2, 3, 11, gpui::Axis::Vertical);

    // Remove 2: the vertical split (2, 3) should collapse to 3
    let tree = tree.remove(2).expect("tree remains");
    assert!(tree.contains(1));
    assert!(!tree.contains(2));
    assert!(tree.contains(3));

    // Remove 3: the horizontal split (1, 3) should collapse to 1
    let tree = tree.remove(3).expect("tree remains");
    assert_eq!(tree, PaneNode::Leaf(1));

    // Remove 1: tree becomes None
    assert!(tree.remove(1).is_none());
}

#[test]
fn visual_split_maximize_close_flow() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("initial frame");

    // Split vertical
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.split_pane(1, gpui::Axis::Horizontal, window, cx);
    });
    cx.draw_frame().expect("frame after vertical split");

    let pane_count = app_state.read_with(&cx, |state, _| {
        state.active_project().unwrap().sessions[0].panes.len()
    });
    assert_eq!(pane_count, 2);

    let new_pane_id = app_state.read_with(&cx, |state, _| state.focused_pane_id.unwrap());

    // Toggle maximize
    app_state.update(&mut cx, |state, cx| {
        state.toggle_maximize_pane(new_pane_id, cx);
    });
    cx.draw_frame().expect("frame while maximized");

    let is_max = app_state.read_with(&cx, |state, _| {
        state.active_project().unwrap().sessions[0].maximized_pane_id
    });
    assert_eq!(is_max, Some(new_pane_id));

    // Restore
    app_state.update(&mut cx, |state, cx| {
        state.toggle_maximize_pane(new_pane_id, cx);
    });
    cx.draw_frame().expect("frame restored");

    // Close the new pane
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.close_pane(new_pane_id, window, cx);
    });
    cx.draw_frame().expect("frame after close");

    let pane_count_after = app_state.read_with(&cx, |state, _| {
        state.active_project().unwrap().sessions[0].panes.len()
    });
    assert_eq!(pane_count_after, 1);
}

#[test]
fn one_mode_keeps_chat_and_panel_separate_from_regular_panes() {
    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    cx.update_window_entity(&state, |state, window, cx| {
        state.settings.agent_layout = false;
        let regular_chat = state.pane_ref(1).unwrap().chat.active;
        state.projects[0].sessions[0].maximized_pane_id = Some(1);
        state.switch_agent_layout(window, cx);
        let chat = state.focused_pane_id.unwrap();
        assert_ne!(chat, 1);
        assert!(state.pane_ref(chat).unwrap().one_mode);
        state.open_tool_panel(chat, window, cx);
        state.new_tool_panel_terminal(chat, window, cx);
        let panel = state.focused_pane_id.unwrap();
        state.set_chat_mode(panel, true, window, cx);
        assert!(!state.pane_ref(panel).unwrap().chat.active);
        state.switch_agent_layout(window, cx);
        assert_eq!(state.active_session().unwrap().maximized_pane_id, Some(1));
        assert_eq!(state.focused_pane_id, Some(1));
        assert_eq!(state.pane_ref(1).unwrap().chat.active, regular_chat);
        assert!(state.pane_ref(chat).unwrap().chat.active);
        let saved = state.saved_workspace(cx);
        let panes = &saved.projects[0].sessions[0].panes;
        assert!(panes.iter().find(|pane| pane.id == chat).unwrap().one_mode);
        assert_eq!(
            panes
                .iter()
                .find(|pane| pane.id == chat)
                .unwrap()
                .regular_focus,
            Some(1)
        );
        state.switch_agent_layout(window, cx);
        assert_eq!(state.focused_pane_id, Some(chat));
        assert_eq!(state.active_session().unwrap().panes.len(), 3);
    });
}

#[test]
fn tool_panel_switches_tabs_without_replacing_terminals() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    let owner = cx.update_window_entity(&app_state, |state, window, cx| {
        state.switch_agent_layout(window, cx);
        state.focused_pane_id.unwrap()
    });
    fn click(cx: &mut gpui::VisualTestContext, selector: &'static str) {
        cx.draw_frame().unwrap();
        let center = cx.debug_bounds(selector).expect(selector).center();
        cx.simulate_mouse_move(center, None, Modifiers::none());
        cx.simulate_click(center, Modifiers::none());
    }
    click(&mut cx, "open-tool-panel");
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("tool-panel-chooser").is_some());
    assert_eq!(
        cx.debug_bounds("tool-panel-choose-browser").is_some(),
        cfg!(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))
    );
    app_state.read_with(&cx, |state, _| {
        assert_eq!(state.active_session().unwrap().panes.len(), 2)
    });
    cx.simulate_keystrokes("t");
    let first = app_state.read_with(&cx, |state, _| state.focused_pane_id.unwrap());
    click(&mut cx, "add-tool-panel-surface");
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("tool-panel-chooser").is_some());
    cx.simulate_keystrokes("escape");
    app_state.read_with(&cx, |state, _| {
        assert_eq!(state.focused_pane_id, Some(first))
    });
    click(&mut cx, "add-tool-panel-surface");
    click(&mut cx, "tool-panel-choose-terminal");
    let second = app_state.read_with(&cx, |state, _| state.focused_pane_id.unwrap());
    assert_ne!(first, second);
    app_state.read_with(&cx, |state, _| {
        let visible = state.visible_pane_ids();
        assert!(!visible.contains(&first));
        assert!(visible.contains(&second));
    });
    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
    let browser = {
        click(&mut cx, "add-tool-panel-surface");
        cx.draw_frame().unwrap();
        cx.simulate_keystrokes("b");
        cx.draw_frame().unwrap();
        assert!(cx.debug_bounds("tool-panel-browser").is_some());
        assert!(cx.debug_bounds("tool-panel-browser-tab").is_some());
        let id = cx.update_window_entity(&app_state, |state, window, cx| {
            let id = state.focused_pane_id.unwrap();
            assert_eq!(state.focused_browser_pane(window, cx), Some(id));
            let pane = state.pane_ref(id).unwrap();
            assert_eq!(pane.display_title.as_ref(), "Browser");
            assert!(pane.pid.is_none());
            assert!(pane.terminal_subscription.is_none());
            assert!(pane.input_state.is_none());
            assert!(state.visible_browser.is_some());
            id
        });
        cx.update_window_entity(&app_state, |state, window, cx| state.open_settings(window, cx));
        cx.draw_frame().unwrap();
        app_state.read_with(&cx, |state, _| assert!(state.visible_browser.is_none()));
        cx.update_window_entity(&app_state, |state, window, cx| state.close_settings(window, cx));
        cx.draw_frame().unwrap();
        app_state.read_with(&cx, |state, _| assert!(state.visible_browser.is_some()));
        click(&mut cx, "hide-tool-panel");
        cx.draw_frame().unwrap();
        assert!(cx.debug_bounds("tool-panel-browser").is_none());
        app_state.read_with(&cx, |state, _| assert!(state.visible_browser.is_none()));
        click(&mut cx, "open-tool-panel");
        click(&mut cx, "add-tool-panel-surface");
        click(&mut cx, "tool-panel-choose-browser");
        app_state.read_with(&cx, |state, _| {
            assert_eq!(state.focused_pane_id, Some(id));
            assert_eq!(
                state.tool_panel_panes(owner).filter(|pane| pane.is_browser()).count(),
                1,
            );
        });
        id
    };
    click(&mut cx, "add-tool-panel-surface");
    click(&mut cx, "tool-panel-choose-files");
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("files-workspace").is_some());
    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
    app_state.read_with(&cx, |state, _| assert!(state.visible_browser.is_none()));
    let files = app_state.read_with(&cx, |state, _| state.focused_pane_id.unwrap());
    click(&mut cx, "hide-tool-panel");
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("tool-panel-tabs").is_none());
    let mut workspace: workspace_state::SavedWorkspace = app_state.read_with(&cx, |state, cx| {
        let saved = state.saved_workspace(cx);
        let panes = &saved.projects[0].sessions[0].panes;
        let saved_owner = panes.iter().find(|pane| pane.id == owner).unwrap();
        assert!(saved_owner.tool_panel_hidden);
        assert_eq!(saved_owner.tool_panel_tab, Some(files));
        for id in [first, second, files] {
            assert_eq!(
                panes
                    .iter()
                    .find(|pane| pane.id == id)
                    .unwrap()
                    .tool_panel_owner,
                Some(owner)
            );
        }
        serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap()
    });
    let expected = workspace_state::SavedBrowser {
        tabs: vec!["https://example.com".into(), "https://example.org".into()],
        active: 1,
    };
    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
    let browser_pane = {
        let saved_browser = workspace.projects[0].sessions[0]
            .panes
            .iter_mut()
            .find(|pane| pane.id == browser)
            .unwrap();
        assert_eq!(saved_browser.tool_panel_owner, Some(owner));
        assert_eq!(saved_browser.display_title, "Browser");
        assert!(
            saved_browser
                .browser
                .as_ref()
                .unwrap()
                .tabs
                .iter()
                .all(String::is_empty)
        );
        saved_browser.browser = Some(expected.clone());
        browser
    };
    #[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))))]
    let browser_pane = {
        let legacy_browser = workspace.projects[0].sessions[0]
            .panes
            .iter_mut()
            .find(|pane| pane.id == second)
            .unwrap();
        legacy_browser.browser = Some(expected);
        legacy_browser.display_title = "Browser".into();
        second
    };
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.startup_loaded = false;
        state.settings.restore.agent_history = false;
        state.apply_saved_workspace(
            RestoreData {
                workspace,
                scrollbacks: HashMap::new(),
                transcripts: HashMap::new(),
                usage: HashMap::new(),
            },
            cx,
        );
        state.ensure_panes_input_state(window, cx);
        let pane = state.pane_ref(browser_pane).unwrap();
        let saved = state.saved_workspace(cx);
        let saved_browser = &saved.projects[0].sessions[0]
            .panes
            .iter()
            .find(|pane| pane.id == browser_pane)
            .unwrap()
            .browser;
        #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            assert_eq!(pane.display_title.as_ref(), "Browser");
            assert!(pane.input_state.is_none());
            assert!(pane.terminal_subscription.is_none());
            assert!(matches!(
                pane.browser.as_ref(),
                Some(ToolPanelBrowser::Saved(_))
            ));
            assert_eq!(saved_browser.as_ref(), Some(&expected));
        }
        #[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))))]
        {
            assert_eq!(pane.display_title.as_ref(), "Terminal");
            assert!(saved_browser.is_none());
        }
    });
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("tool-panel-tabs").is_none());
    click(&mut cx, "open-tool-panel");
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("files-workspace").is_some());
    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
    app_state.read_with(&cx, |state, _| {
        assert!(state.visible_browser.is_none());
        assert!(state.pane_ref(browser).unwrap().mounted_browser().is_none());
    });
    #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = state.pane_mut(browser).unwrap();
        let browser = AppState::mount_browser(pane, window, cx).unwrap();
        assert_eq!(browser.view.read(cx).snapshot(), expected);
    });
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.select_tool_panel_tab(owner, second, window, cx);
        state.open_project_diff(second, window, cx);
        let changes = state.focused_pane_id.unwrap();
        assert_ne!(changes, second);
        assert!(state.pane_ref(changes).unwrap().diff_preview.is_some());
        state.select_tool_panel_tab(owner, second, window, cx);
        state.close_pane(changes, window, cx);
        assert_eq!(state.focused_pane_id, Some(second));
        state.close_pane(second, window, cx);
        assert_eq!(state.focused_pane_id, Some(first));
        #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            state.close_pane(browser, window, cx);
            assert!(state.pane_ref(browser).is_none());
            assert_eq!(state.focused_pane_id, Some(first));
        }
        state.switch_agent_layout(window, cx);
        state.focus_pane(first, window, cx);
        assert_eq!(state.focused_pane_id, Some(1));
    });
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("tool-panel-tabs").is_none());
    assert!(cx.debug_bounds("open-tool-panel").is_none());
}

#[test]
fn project_diff_opens_beside_source_and_renders_expanded_patch() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.open_project_diff(1, window, cx);
        let id = state.focused_pane_id.unwrap();
        let view = state.pane_mut(id).unwrap().diff_preview.as_mut().unwrap();
        view.request += 1; // Keep the asynchronous Git result from replacing this fixture.
        view.expanded.insert("example.rs".to_owned());
        view.result = Some(Ok(project_diff::ProjectDiff {
            root: std::env::temp_dir(),
            files: vec![
                project_diff::DiffFile {
                    group: project_diff::FileGroup::Tracked,
                    path: "example.rs".into(),
                    additions: Some(1),
                    deletions: Some(1),
                    patch: "@@ -1,2 +1,2 @@\n fn main() {}\n-old\n+new\n".into(),
                },
                project_diff::DiffFile {
                    group: project_diff::FileGroup::Untracked,
                    path: ".new-config".into(),
                    additions: Some(1),
                    deletions: Some(0),
                    patch: "+new\n".into(),
                },
            ],
        }));
    });
    cx.draw_frame().expect("diff preview draws");
    cx.run_until_parked();
    cx.draw_frame().expect("highlighted diff preview draws");
    let patch = cx.debug_bounds("inline-diff").expect("inline patch");
    let context = cx
        .debug_bounds("inline-diff-context")
        .expect("context line");
    let removed = cx
        .debug_bounds("inline-diff-removed")
        .expect("removed line");
    let added = cx.debug_bounds("inline-diff-added").expect("added line");
    assert_eq!(removed.origin.y, context.origin.y + context.size.height);
    assert_eq!(added.origin.y, removed.origin.y + removed.size.height);
    assert!(added.size.width >= patch.size.width - px(1.0));
    let section = cx
        .debug_bounds("project-diff-section-Untracked")
        .expect("untracked section");
    cx.simulate_click(section.center(), Modifiers::default());
    cx.draw_frame()
        .expect("diff preview draws after collapsing");
    app_state.read_with(&cx, |state, _| {
        let view = state
            .pane_ref(state.focused_pane_id.unwrap())
            .unwrap()
            .diff_preview
            .as_ref()
            .unwrap();
        assert!(view.untracked_collapsed);
        assert!(!view.tracked_collapsed);
    });
    app_state.read_with(&cx, |state, _| {
        let session = &state.active_project().unwrap().sessions[0];
        assert_eq!(session.panes.len(), 2);
        assert!(session.panes.iter().any(|pane| pane.diff_preview.is_some()));
    });
}

#[test]
fn repeated_links_and_changes_reuse_the_same_preview_pane() {
    let dir = std::env::temp_dir().join(format!("shob-reuse-preview-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let first = dir.join("first.md");
    let second = dir.join("second.md");
    let code = dir.join("code.rs");
    std::fs::write(&first, "# First\n").unwrap();
    std::fs::write(&second, "# Second\n").unwrap();
    std::fs::write(&code, "fn main() {}\n").unwrap();
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    let preview_id = cx.update_window_entity(&app_state, |state, window, cx| {
        assert!(state.open_local_link(1, first.to_str().unwrap(), window, cx));
        let id = state.focused_pane_id.unwrap();
        assert!(state.open_local_link(1, second.to_str().unwrap(), window, cx));
        assert_eq!(state.focused_pane_id, Some(id));
        assert!(state.open_local_link(id, code.to_str().unwrap(), window, cx));
        state.open_project_diff(1, window, cx);
        assert!(state.open_local_link(1, second.to_str().unwrap(), window, cx));
        id
    });
    cx.run_until_parked();
    cx.draw_frame().expect("reused preview draws");
    app_state.read_with(&cx, |state, _| {
        let session = &state.active_project().unwrap().sessions[0];
        assert_eq!(session.panes.len(), 2);
        let preview = session
            .panes
            .iter()
            .find(|pane| pane.id == preview_id)
            .unwrap();
        assert_eq!(preview.preview_owner, Some(1));
        assert_eq!(preview.markdown_preview.as_ref().unwrap().0, second);
        assert!(preview.diff_preview.is_none());
    });
    let close = cx
        .debug_bounds("close-preview-tab")
        .expect("Markdown tab close");
    cx.simulate_click(close.center(), Modifiers::default());
    app_state.read_with(&cx, |state, _| {
        assert_eq!(state.active_project().unwrap().sessions[0].panes.len(), 1);
    });
    cx.update_window_entity(&app_state, |state, window, cx| {
        assert!(state.open_local_link(1, first.to_str().unwrap(), window, cx));
    });
    app_state.read_with(&cx, |state, _| {
        assert_eq!(state.active_project().unwrap().sessions[0].panes.len(), 2)
    });
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn local_file_url_opens_editor_beside_agent() {
    let dir = std::env::temp_dir().join(format!("shob-local-link-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("App.svelte");
    std::fs::write(&path, "<h1>Hello</h1>\n").unwrap();
    let file_url = url::Url::from_file_path(&path).unwrap().to_string();
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.update_window_entity(&app_state, |state, window, cx| {
        assert!(!state.open_local_link(1, "https://example.com/App.svelte", window, cx));
        assert!(state.open_local_link(1, &file_url, window, cx));
    });
    cx.run_until_parked();
    cx.draw_frame().expect("local file view draws");
    app_state.read_with(&cx, |state, cx| {
        let session = &state.active_project().unwrap().sessions[0];
        assert_eq!(session.panes.len(), 2);
        let view = session
            .panes
            .iter()
            .find_map(|pane| pane.diff_preview.as_ref())
            .unwrap();
        assert!(view.file_only);
        match &view.file_view.as_ref().unwrap().state {
            ReadOnlyFileState::Ready { input, .. } => {
                assert_eq!(input.read(cx).value().to_string(), "<h1>Hello</h1>\n")
            }
            _ => panic!("local file did not load"),
        }
    });
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn clicking_changed_file_opens_read_only_editor() {
    let dir = std::env::temp_dir().join(format!("shob-click-file-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("example.rs"), "fn main() {}\n").unwrap();
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.open_project_diff(1, window, cx);
        let id = state.focused_pane_id.unwrap();
        let view = state.pane_mut(id).unwrap().diff_preview.as_mut().unwrap();
        view.request += 1;
        view.result = Some(Ok(project_diff::ProjectDiff {
            root: dir.clone(),
            files: vec![project_diff::DiffFile {
                group: project_diff::FileGroup::Tracked,
                path: "example.rs".into(),
                additions: Some(1),
                deletions: Some(0),
                patch: "+fn main() {}\n".into(),
            }],
        }));
    });
    cx.draw_frame().expect("changes draws");
    let row = cx.debug_bounds("project-diff-file-0").expect("file row");
    cx.simulate_click(row.origin + point(px(90.0), px(10.0)), Modifiers::default());
    cx.run_until_parked();
    cx.draw_frame().expect("file view draws");
    app_state.read_with(&cx, |state, cx| {
        let view = state
            .pane_ref(state.focused_pane_id.unwrap())
            .unwrap()
            .diff_preview
            .as_ref()
            .unwrap();
        match &view.file_view.as_ref().expect("file view opened").state {
            ReadOnlyFileState::Ready { input, .. } => {
                assert_eq!(input.read(cx).value().to_string(), "fn main() {}\n")
            }
            _ => panic!("file did not finish loading"),
        }
    });
    let close = cx
        .debug_bounds("close-preview-tab")
        .expect("file tab close");
    cx.simulate_click(close.center(), Modifiers::default());
    cx.draw_frame().expect("changes draws after closing file");
    assert!(cx.debug_bounds("project-diff-file-0").is_some());
    assert!(cx.debug_bounds("close-preview-tab").is_none());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn read_only_file_editor_accepts_selection_but_not_edits() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    let input = cx.update_window_entity(&app_state, |state, window, cx| {
        state.open_project_diff(1, window, cx);
        let id = state.focused_pane_id.unwrap();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("rust")
                .line_number(true)
                .default_value("original text")
                .read_only(true)
        });
        let view = state.pane_mut(id).unwrap().diff_preview.as_mut().unwrap();
        view.file_view = Some(ReadOnlyFile {
            path: "test.rs".into(),
            state: ReadOnlyFileState::Ready {
                input: input.clone(),
                deleted: false,
            },
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        input
    });
    cx.draw_frame().expect("file editor draws");
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("replaced");
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    assert_eq!(
        input.read_with(&cx, |input, _| input.value().to_string()),
        "original text"
    );
}

#[test]
fn dragging_sidebar_edge_resizes_and_clamps_the_width() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("initial frame");

    let handle = cx
        .debug_bounds("sidebar-resize-handle")
        .expect("sidebar resize handle");
    let grab = point(
        handle.origin.x + handle.size.width / 2.0,
        handle.origin.y + px(200.0),
    );

    // Drag right: the sidebar must widen by exactly the pointer delta.
    let start_width = app_state.read_with(&cx, |state, _| state.sidebar_width);
    cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        point(grab.x + px(80.0), grab.y),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    cx.draw_frame().expect("frame while widening");
    let widened = app_state.read_with(&cx, |state, _| state.sidebar_width);
    assert_eq!(
        widened,
        start_width + px(80.0),
        "the edge must follow the pointer one-to-one"
    );

    // Crossing the collapse threshold previews off-canvas without storing a sub-minimum width.
    cx.simulate_mouse_move(
        point(px(10.0), grab.y),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    cx.draw_frame().expect("frame while clamping");
    let narrowed = app_state.read_with(&cx, |state, _| state.sidebar_width);
    assert_eq!(narrowed, SIDEBAR_MIN_WIDTH, "the minimum width must hold");
    assert!(cx.debug_bounds("sidebar").is_none());

    // Dragging back restores the rail and leaves 640 px for the workspace.
    cx.simulate_mouse_move(
        point(px(1150.0), grab.y),
        Some(MouseButton::Left),
        Modifiers::none(),
    );
    cx.draw_frame().expect("frame at the ceiling");
    let viewport_width = cx.update(|window, _| window.viewport_size().width);
    let widest = app_state.read_with(&cx, |state, _| state.sidebar_width);
    assert_eq!(widest, viewport_width - SIDEBAR_MAIN_MIN_WIDTH);

    cx.simulate_mouse_up(grab, MouseButton::Left, Modifiers::none());
    cx.draw_frame().expect("frame after releasing");
    app_state.read_with(&cx, |state, _| {
        assert!(!state.sidebar_resizing, "the drag must be marked finished");
        assert_eq!(state.sidebar_resize_offset, None);
    });

    let edge = cx.debug_bounds("sidebar-resize-handle").unwrap().center();
    cx.simulate_mouse_down(edge, MouseButton::Left, Modifiers::none());
    let collapsed = point(px(10.0), edge.y);
    cx.simulate_mouse_move(collapsed, Some(MouseButton::Left), Modifiers::none());
    cx.simulate_mouse_up(collapsed, MouseButton::Left, Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(!app_state.read_with(&cx, |state, _| state.sidebar_visible));
}

#[test]
fn dragging_split_terminal_divider_resizes_without_panic() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("initial frame");

    // Split vertical
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.split_pane(1, gpui::Axis::Horizontal, window, cx);
    });
    cx.draw_frame().expect("frame after vertical split");

    let y = px(400.0);
    let grab = point(px(700.0), y);
    cx.simulate_mouse_down(grab, MouseButton::Left, Modifiers::none());
    for dx in [-150.0, -50.0, 0.0, 50.0, 150.0] {
        cx.simulate_mouse_move(
            point(px(700.0 + dx), y),
            Some(MouseButton::Left),
            Modifiers::none(),
        );
        cx.draw_frame().expect("frame while dragging divider");
    }
    cx.simulate_mouse_up(grab, MouseButton::Left, Modifiers::none());
    cx.draw_frame().expect("frame after dragging divider");
}

#[test]
fn reproduce_nested_split_bounds() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("initial frame");

    // Split 1 vertically: Left: 1, Right: new (2)
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.split_pane(1, gpui::Axis::Horizontal, window, cx);
    });
    cx.draw_frame().expect("frame after vertical split");

    // Split 1 horizontally: Top-left: 1, Bottom-left: new (3)
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.split_pane(1, gpui::Axis::Vertical, window, cx);
    });
    cx.run_until_parked();
    cx.draw_frame().expect("frame after horizontal split");

    // Check terminal dimensions/bounds for all 3 panes
    let (term1, term2, term3) = app_state.read_with(&cx, |state, cx| {
        let session = &state.active_project().unwrap().sessions[0];
        let t1 = session.panes[0].terminal.read(cx).dimensions();
        let t2 = session.panes[1].terminal.read(cx).dimensions();
        let t3 = session.panes[2].terminal.read(cx).dimensions();
        (t1, t2, t3)
    });

    // Ensure no terminal collapsed to height 0 / 1 row
    assert!(term1.1 > 5, "term1 collapsed: {:?}", term1);
    assert!(term2.1 > 5, "term2 collapsed: {:?}", term2);
    assert!(term3.1 > 5, "term3 collapsed: {:?}", term3);
}

#[test]
fn horizontal_then_vertical_nested_split_does_not_collapse() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("initial frame");

    // Split 1 horizontally: Top: 1, Bottom: 2
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.split_pane(1, gpui::Axis::Vertical, window, cx);
    });
    cx.draw_frame().expect("frame after horizontal split");

    let bottom_id = app_state.read_with(&cx, |state, _| state.focused_pane_id.unwrap());

    // Split bottom vertically: Bottom-left: 2, Bottom-right: 3
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.split_pane(bottom_id, gpui::Axis::Horizontal, window, cx);
    });
    cx.run_until_parked();
    cx.draw_frame().expect("frame after vertical split");

    let (term1, term2, term3) = app_state.read_with(&cx, |state, cx| {
        let session = &state.active_project().unwrap().sessions[0];
        let t1 = session.panes[0].terminal.read(cx).dimensions();
        let t2 = session.panes[1].terminal.read(cx).dimensions();
        let t3 = session.panes[2].terminal.read(cx).dimensions();
        (t1, t2, t3)
    });

    assert!(term1.1 > 5, "term1 collapsed: {:?}", term1);
    assert!(term2.1 > 5, "term2 collapsed: {:?}", term2);
    assert!(term3.1 > 5, "term3 collapsed: {:?}", term3);
}

#[test]
fn session_isolation_switch_tabs_keeps_screens_separate() {
    // The Shob-style isolation contract: creating a second session must
    // not touch the first session's pane tree, and each tab renders its
    // own screen.
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("initial frame");

    cx.update_window_entity(&app_state, |state, window, cx| {
        state.split_pane(1, gpui::Axis::Horizontal, window, cx);
    });
    cx.draw_frame().expect("frame after split in session 1");

    let new_pane_id = app_state.read_with(&cx, |state, _| state.focused_pane_id.unwrap());

    cx.update_window_entity(&app_state, |state, window, cx| {
        state.create_new_session_in_project(0, window, cx);
    });
    cx.draw_frame().expect("frame after adding a session");

    let (first_pane_count, second_tree) = app_state.read_with(&cx, |state, _| {
        let project = state.active_project().unwrap();
        (
            project.sessions[0].panes.len(),
            project.sessions[1].pane_tree.clone(),
        )
    });
    assert_eq!(first_pane_count, 2);
    // The new session starts on its own full-size screen with one pane.
    assert!(matches!(second_tree, Some(PaneNode::Leaf(_))));

    // Switch back to the first tab: the split is still there.
    app_state.update(&mut cx, |state, cx| {
        state.active_project_mut().unwrap().active_session_idx = 0;
        cx.notify();
    });
    cx.draw_frame().expect("frame after switching tabs");
    let first_tree = app_state.read_with(&cx, |state, _| {
        state.active_project().unwrap().sessions[0]
            .pane_tree
            .clone()
    });
    assert!(first_tree.is_some_and(|t| t.contains(1) && t.contains(new_pane_id)));
}
/// A project directory that is not the repository the test runs in, so
/// opening it really is "a second project".
fn scratch_project_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(name);
    std::fs::create_dir_all(&dir).expect("scratch project dir");
    dir
}

#[test]
fn opening_a_second_project_keeps_the_first_one_alive() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("initial frame");

    let dir = scratch_project_dir("shob-multi-project-open");
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.open_project_path(dir, window, cx);
    });
    cx.draw_frame().expect("frame with two projects");

    let (project_count, active_idx, first_sessions, second_sessions, expanded) = app_state
        .read_with(&cx, |state, _| {
            (
                state.projects.len(),
                state.active_project_idx,
                state.projects[0].sessions.len(),
                state.projects[1].sessions.len(),
                state.projects[1].expanded,
            )
        });
    assert_eq!(project_count, 2);
    assert_eq!(active_idx, 1);
    assert_eq!(first_sessions, 1, "the first project keeps its session");
    assert_eq!(second_sessions, 1, "the new project starts with a session");
    assert!(expanded, "the newly opened project is expanded");
}

#[test]
fn reordering_projects_preserves_selection_and_saved_order() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.open_project_path(scratch_project_dir("shob-reorder-second"), window, cx);
        state.open_project_path(scratch_project_dir("shob-reorder-third"), window, cx);
    });
    app_state.update(&mut cx, |state, cx| {
        let ids: Vec<_> = state.projects.iter().map(|project| project.id).collect();
        state.reorder_project(ids[0], ids[2], cx);
        assert_eq!(
            state.projects.iter().map(|p| p.id).collect::<Vec<_>>(),
            [ids[1], ids[2], ids[0]]
        );
        assert_eq!(state.active_project().unwrap().id, ids[2]);
        let saved = serde_json::to_string(&state.saved_workspace(cx)).unwrap();
        let mut restored: workspace_state::SavedWorkspace = serde_json::from_str(&saved).unwrap();
        restored.normalize();
        assert_eq!(
            restored.projects.iter().map(|p| p.id).collect::<Vec<_>>(),
            [ids[1], ids[2], ids[0]]
        );
        state.reorder_project(ids[0], ids[1], cx);
        assert_eq!(state.projects.iter().map(|p| p.id).collect::<Vec<_>>(), ids);
        assert_eq!(state.active_project().unwrap().id, ids[2]);
    });
}

#[test]
fn sessions_are_created_in_the_project_that_asks_for_them() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("initial frame");

    let dir = scratch_project_dir("shob-multi-project-sessions");
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.open_project_path(dir, window, cx);
    });
    cx.draw_frame().expect("frame with two projects");

    // Create a session in the *first* project while the second one is the
    // active project: the new terminal must land in project 0 and switch
    // the workspace over to it.
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.create_new_session_in_project(0, window, cx);
    });
    cx.draw_frame().expect("frame after creating a session");

    let (first, second, active) = app_state.read_with(&cx, |state, _| {
        (
            state.projects[0].sessions.len(),
            state.projects[1].sessions.len(),
            state.active_project_idx,
        )
    });
    assert_eq!(first, 2);
    assert_eq!(second, 1);
    assert_eq!(active, 0);
}

#[test]
fn closing_a_session_or_project_leaves_the_others_intact() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("initial frame");

    let dir = scratch_project_dir("shob-multi-project-close");
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.open_project_path(dir, window, cx);
    });
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.create_new_session_in_project(0, window, cx);
    });
    cx.draw_frame()
        .expect("frame with two projects and two sessions");

    // Close the extra session in project 0. Project 1 must be untouched.
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.close_session(0, 1, window, cx);
    });
    cx.draw_frame().expect("frame after closing a session");
    let (first, second) = app_state.read_with(&cx, |state, _| {
        (
            state.projects[0].sessions.len(),
            state.projects[1].sessions.len(),
        )
    });
    assert_eq!((first, second), (1, 1));

    // Close the first project. The second one becomes active with its own
    // session still running.
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.close_project(0, window, cx);
    });
    cx.draw_frame().expect("frame after closing a project");
    let (project_count, active_sessions, name) = app_state.read_with(&cx, |state, _| {
        (
            state.projects.len(),
            state.active_session().map(|s| s.panes.len()).unwrap_or(0),
            state.projects[0].name.to_string(),
        )
    });
    assert_eq!(project_count, 1);
    assert_eq!(active_sessions, 1);
    assert_eq!(name, "shob-multi-project-close");
}
