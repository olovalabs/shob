use super::*;

#[test]
fn tree_sorts_directories_first_and_expands_only_requested_directories() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
    std::fs::write(dir.path().join(".hidden"), "").unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    let (rows, truncated) = tree(dir.path(), &BTreeSet::new()).unwrap();
    assert!(!truncated);
    assert_eq!(
        rows.iter().map(|e| e.path.as_path()).collect::<Vec<_>>(),
        [
            Path::new("src"),
            Path::new(".hidden"),
            Path::new("Cargo.toml")
        ]
    );
    let (rows, _) = tree(dir.path(), &BTreeSet::from([PathBuf::from("src")])).unwrap();
    assert_eq!(rows[1].path, Path::new("src/main.rs"));
    assert_eq!(rows[1].depth, 1);
    assert!(read_text(dir.path(), Path::new("src")).is_err());
    assert!(read_text(dir.path(), Path::new("../outside")).is_err());
}

#[test]
fn saves_preserve_text_and_reject_external_changes_binary_and_oversized_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = Path::new("file.rs");
    let original = "// original\r\nfn main() {}\r\n";
    std::fs::write(dir.path().join(path), original).unwrap();
    let edited = "// edited\r\nfn main() {}\r\n";
    save(dir.path(), path, original, edited).unwrap();
    assert_eq!(read_text(dir.path(), path).unwrap(), edited);
    assert!(
        save(dir.path(), path, original, "overwrite")
            .unwrap_err()
            .contains("changed on disk")
    );
    assert_eq!(read_text(dir.path(), path).unwrap(), edited);
    std::fs::write(dir.path().join(path), [0, 1]).unwrap();
    assert!(read_text(dir.path(), path).unwrap_err().contains("Binary"));
    std::fs::File::create(dir.path().join(path))
        .unwrap()
        .set_len(MAX_FILE_BYTES as u64 + 1)
        .unwrap();
    assert!(read_text(dir.path(), path).unwrap_err().contains("1 MiB"));
}

#[test]
fn git_status_and_operations_preserve_literal_paths_and_worktree_contents() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run(root, &["init", "-q"]).unwrap();
    run(root, &["config", "user.name", "Test"]).unwrap();
    run(root, &["config", "user.email", "test@example.com"]).unwrap();
    let name = "[literal] file.txt";
    std::fs::write(root.join(name), "initial\n").unwrap();
    run(root, &["add", "--", name]).unwrap();
    let repo = repository(root).unwrap();
    assert!(repo.changes[0].staged);
    run(root, &["rm", "--cached", "--", name]).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join(name)).unwrap(),
        "initial\n"
    );
    run(root, &["add", "--", name]).unwrap();
    run(root, &["commit", "-qm", "initial"]).unwrap();
    std::fs::write(root.join(name), "staged\n").unwrap();
    run(root, &["add", "--", name]).unwrap();
    std::fs::write(root.join(name), "working\n").unwrap();
    let repo = repository(root).unwrap();
    assert_eq!(repo.changes[0].path, Path::new(name));
    assert!(repo.changes[0].staged && repo.changes[0].unstaged);
    run(root, &["reset", "-q", "HEAD", "--", name]).unwrap();
    assert!(!repository(root).unwrap().changes[0].staged);
    assert_eq!(
        std::fs::read_to_string(root.join(name)).unwrap(),
        "working\n"
    );
    let renamed = parse_status("R  new name\0old name\0?? new\nfile\0").unwrap();
    assert_eq!(renamed.len(), 2);
    assert_eq!(renamed[1].path, Path::new("new\nfile"));
    assert!(parse_status("M  ../outside\0").is_err());
}

#[test]
fn files_workspace_restores_drafts_and_renders_both_themes() {
    use crate::resize_tests::{DrawFrame as _, TestAppContext, open_app};
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;
    let fixture = tempfile::tempdir().unwrap();
    let captures = tempfile::tempdir().unwrap();
    std::fs::create_dir(fixture.path().join("src")).unwrap();
    std::fs::write(fixture.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    let other_checkout = tempfile::tempdir().unwrap();
    std::fs::create_dir(other_checkout.path().join("src")).unwrap();
    std::fs::write(other_checkout.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let root = fixture.path().to_path_buf();
    run(&root, &["init", "-q"]).unwrap();
    run(&root, &["config", "user.name", "Test"]).unwrap();
    run(&root, &["config", "user.email", "test@example.com"]).unwrap();
    run(&root, &["add", "."]).unwrap();
    run(&root, &["commit", "-qm", "initial"]).unwrap();
    cx.update_window_entity(&state, |state, window, cx| {
        state.projects[0].path = root.clone();
        state.settings.agent_layout = true;
        let chat = &mut state.pane_mut(1).unwrap().chat;
        chat.active = true;
        chat.workspace_override = Some(root.clone());
        state.open_tool_panel(1, window, cx);
    });
    cx.draw_frame().unwrap();
    cx.update(|window, _| {
        window
            .render_to_image()
            .unwrap()
            .save(captures.path().join("before.png"))
            .unwrap()
    });
    cx.update_window_entity(&state, |state, window, cx| {
        let panel = state.focused_pane_id.unwrap();
        state.open_files_surface(panel, window, cx);
    });
    cx.run_until_parked();
    let view = state.read_with(&cx, |state, _| {
        state
            .pane_ref(state.focused_pane_id.unwrap())
            .unwrap()
            .files_surface
            .as_ref()
            .unwrap()
            .0
            .clone()
    });
    cx.update_window_entity(&view, |view, window, cx| {
        view.expanded.insert("src".into());
        view.refresh(cx);
        view.open(
            SavedFile {
                path: "src/main.rs".into(),
                ..Default::default()
            },
            true,
            window,
            cx,
        );
    });
    cx.run_until_parked();
    cx.update_window_entity(&view, |view, window, cx| {
        view.editors[0].input.update(cx, |input, cx| {
            input.set_value("fn main() { println!(\"draft\"); }\n", window, cx);
            let before = input.value();
            let oversized = "é".repeat(MAX_FILE_BYTES);
            use gpui::EntityInputHandler as _;
            input.replace_text_in_range(None, &oversized, window, cx);
            assert_eq!(input.value(), before);
            input.replace_and_mark_text_in_range(None, &oversized, None, window, cx);
            assert_eq!(input.value(), before);
        });
    });
    cx.run_until_parked();
    cx.update_window_entity(&view, |view, _, cx| assert!(!view.prepare_close(cx)));
    cx.update_window_entity(&state, |state, window, cx| {
        state.close_project(0, window, cx);
        assert_eq!(
            state.projects.len(),
            1,
            "a dirty Files draft blocks project close"
        );
    });
    let saved = view.read_with(&cx, |view, cx| view.snapshot(cx));
    assert_eq!(saved.tabs[0].baseline.as_deref(), Some("fn main() {}\n"));
    assert!(saved.tabs[0].draft.as_ref().unwrap().contains("draft"));
    let mut saved: SavedFiles =
        serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap();
    saved.selected = Some("missing.rs".into());
    saved.tabs.insert(
        0,
        SavedFile {
            path: "missing.rs".into(),
            ..Default::default()
        },
    );
    #[cfg(unix)]
    saved.tabs.insert(
        0,
        SavedFile {
            path: std::ffi::OsString::from_vec(vec![0xff]).into(),
            ..Default::default()
        },
    );
    cx.update_window_entity(&state, |state, window, cx| {
        let panel = state.focused_pane_id.unwrap();
        state.projects[0].path = other_checkout.path().to_path_buf();
        let pane = state.pane_mut(panel).unwrap();
        pane.files_surface = None;
        pane.files_restore = Some(saved);
        state.ensure_panes_input_state(window, cx);
    });
    cx.run_until_parked();
    for (preference, name) in [
        (crate::ThemePreference::Dark, "dark"),
        (crate::ThemePreference::Light, "light"),
    ] {
        cx.update_window_entity(&state, |state, window, cx| {
            state.theme_preference = preference;
            state.apply_theme_preference(Some(window), cx);
        });
        cx.run_until_parked();
        cx.draw_frame().unwrap();
        assert!(cx.debug_bounds("files-workspace").is_some());
        cx.update(|window, _| {
            window
                .render_to_image()
                .unwrap()
                .save(captures.path().join(format!("after-{name}.png")))
                .unwrap()
        });
    }
    state.read_with(&cx, |state, cx| {
        let pane = state.pane_ref(state.focused_pane_id.unwrap()).unwrap();
        let restored = pane.files_surface.as_ref().unwrap().0.read(cx);
        assert_eq!(
            restored.editors[0].input.read(cx).value(),
            "fn main() { println!(\"draft\"); }\n"
        );
        assert!(restored.expanded.contains(Path::new("src")));
        assert_eq!(
            std::fs::read_to_string(root.join("src/main.rs")).unwrap(),
            "fn main() {}\n"
        );
    });
    cx.draw_frame().unwrap();
    cx.simulate_keystrokes("ctrl-s");
    cx.run_until_parked();
    assert_eq!(
        std::fs::read_to_string(root.join("src/main.rs")).unwrap(),
        "fn main() { println!(\"draft\"); }\n"
    );
    assert_eq!(
        std::fs::read_to_string(other_checkout.path().join("src/main.rs")).unwrap(),
        "fn main() {}\n"
    );
    let restored = state.read_with(&cx, |state, _| {
        state
            .pane_ref(state.focused_pane_id.unwrap())
            .unwrap()
            .files_surface
            .as_ref()
            .unwrap()
            .0
            .clone()
    });
    cx.update_window_entity(&restored, |view, _, cx| {
        view.git = true;
        view.refresh(cx);
    });
    cx.run_until_parked();
    cx.update_window_entity(&restored, |view, window, cx| {
        assert!(!view.editors[0].dirty);
        assert!(view.prepare_close(cx));
        let change = view.repository.as_ref().unwrap().changes[0].clone();
        view.show_diff(change, window, cx);
    });
    cx.run_until_parked();
    restored.read_with(&cx, |view, cx| {
        assert!(
            view.diff
                .as_ref()
                .unwrap()
                .input
                .read(cx)
                .value()
                .contains("+fn main()")
        )
    });
    cx.draw_frame().unwrap();
    cx.update(|window, _| {
        window
            .render_to_image()
            .unwrap()
            .save(captures.path().join("git.png"))
            .unwrap()
    });

    cx.update_window_entity(&restored, |view, window, cx| {
        view.show_diff(
            Change {
                path: "nonexistent.txt".into(),
                status: "??".into(),
                staged: false,
                unstaged: true,
            },
            window,
            cx,
        );
    });
    cx.run_until_parked();
    restored.read_with(&cx, |view, cx| {
        let displayed = view.diff.as_ref().unwrap();
        assert_eq!(displayed.repository_path, Path::new("src/main.rs"));
        assert_eq!(
            displayed.edit_path.as_deref(),
            Some(Path::new("src/main.rs"))
        );
        assert!(displayed.input.read(cx).value().contains("+fn main()"));
    });

    cx.update_window_entity(&restored, |view, window, cx| {
        let change = view.repository.as_ref().unwrap().changes[0].clone();
        view.show_diff(change, window, cx);
        view.select_editor("src/main.rs".into(), window, cx);
    });
    cx.run_until_parked();
    restored.read_with(&cx, |view, _| {
        assert_eq!(view.selected.as_deref(), Some(Path::new("src/main.rs")));
        assert!(view.diff.is_none());
    });
}
