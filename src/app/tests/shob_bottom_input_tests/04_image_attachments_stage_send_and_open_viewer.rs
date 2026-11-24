#[test]
fn image_attachments_stage_send_and_open_viewer() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    let path = std::env::temp_dir().join(format!("shob-ui-attachment-{}.png", std::process::id()));
    image::RgbaImage::from_pixel(4, 3, image::Rgba([200, 80, 20, 255]))
        .save(&path)
        .expect("write test image");
    let attachment = attachments::from_path(&path).expect("test image is valid");
    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

    cx.update_window_entity(&app_state, |state, window, cx| {
        assert!(state.attach_image(pane_id, attachment, window, cx));
    });
    cx.draw_frame().expect("frame with attachment strip");

    app_state.read_with(&cx, |state, _| {
        let pane = &state.projects[0].sessions[0].panes[0];
        assert_eq!(pane.attachments.len(), 1);
    });

    // Submitting sends the path with the prompt and clears the strip;
    // command history keeps only what the user typed.
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |input, cx| input.set_value("what is this", window, cx));
        state.execute_pane_input(pane_id, window, cx);
    });
    wait_for_terminal_submission(&mut cx, &app_state, pane_id);

    app_state.read_with(&cx, |state, _| {
        let pane = &state.projects[0].sessions[0].panes[0];
        assert!(pane.attachments.is_empty());
        assert_eq!(pane.command_history, vec!["what is this"]);
    });

    let attachment = attachments::from_path(&path).expect("test image is still valid");
    cx.update_window_entity(&app_state, |state, _window, cx| {
        state.open_image_viewer(&attachment, cx);
    });
    cx.draw_frame().expect("frame with image viewer");
    app_state.read_with(&cx, |state, _| {
        assert_eq!(
            state.image_viewer.as_ref().map(|viewer| viewer.dimensions),
            Some(Some((4, 3)))
        );
    });

    cx.update_window_entity(&app_state, |state, _window, cx| {
        state.image_viewer = None;
        cx.notify();
    });
    cx.draw_frame().expect("frame after closing viewer");

    std::fs::remove_file(path).ok();
}

#[test]
fn copied_image_path_attaches_only_inside_an_agent() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    let path =
        std::env::temp_dir().join(format!("shob-copied-image-path-{}.png", std::process::id()));
    image::RgbaImage::from_pixel(2, 2, image::Rgba([5, 90, 150, 255]))
        .save(&path)
        .expect("write test image");
    let item = gpui::ClipboardItem::new_string(path.display().to_string());
    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

    // A shell pane keeps a copied path as text.
    cx.update_window_entity(&app_state, |state, window, cx| {
        assert!(!state.attach_images_from_item(pane_id, &item, window, cx));
    });

    // An agent pane recognizes it as an image and stages a thumbnail.
    cx.update_window_entity(&app_state, |state, window, cx| {
        state.projects[0].sessions[0].panes[0].is_agent = true;
        assert!(state.attach_images_from_item(pane_id, &item, window, cx));
        assert_eq!(state.projects[0].sessions[0].panes[0].attachments.len(), 1);
    });
    cx.draw_frame().expect("frame with path-derived attachment");

    std::fs::remove_file(path).ok();
}

#[test]
fn ctrl_c_cancels_draft_and_interrupts_running_process() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

    // 1. Typing draft and pressing Ctrl+C clears draft and sends SIGINT
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("some draft command", window, cx);
        });
        let ev = gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("ctrl-c").unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        state.handle_pane_input_key(pane_id, &ev, window, cx);
    });

    app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        assert_eq!(
            input.read(cx).value().as_str(),
            "",
            "Ctrl+C must clear input draft"
        );
    });

    // 2. Pressing Ctrl+C while command is working interrupts and restores idle
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        pane.status = SessionStatus::Working;
        state.handle_pane_ctrl_c(pane_id, window, cx);
    });

    app_state.read_with(&cx, |state, _| {
        let pane = &state.projects[0].sessions[0].panes[0];
        assert_eq!(
            pane.status,
            SessionStatus::Idle,
            "Ctrl+C must interrupt working command"
        );
        assert!(
            pane.bottom_input_visible,
            "Bottom input must be restored after Ctrl+C"
        );
    });
}

#[test]
fn native_terminal_readline_shortcuts_in_input() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

    // Test Ctrl+U (clear line)
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("command to clear", window, cx);
        });
        let ev = gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("ctrl-u").unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        state.handle_pane_input_key(pane_id, &ev, window, cx);
    });

    app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        assert_eq!(
            input.read(cx).value().as_str(),
            "",
            "Ctrl+U must clear line"
        );
    });

    // Test Ctrl+W (delete previous word)
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("git checkout main", window, cx);
        });
        let ev = gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("ctrl-w").unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        state.handle_pane_input_key(pane_id, &ev, window, cx);
    });

    app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        assert_eq!(
            input.read(cx).value().trim_end(),
            "git checkout",
            "Ctrl+W must delete last word"
        );
    });
}

#[test]
fn autosuggestion_tab_and_right_arrow_completion() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

    // Populate history
    cx.update_window_entity(&app_state, |state, _window, _cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        pane.command_history
            .push("docker compose up -d".to_string());
    });

    // Type prefix "doc" and hit Tab
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("doc", window, cx);
        });
        let ev = gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("tab").unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        state.handle_pane_input_key(pane_id, &ev, window, cx);
    });

    app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        assert_eq!(
            input.read(cx).value().as_str(),
            "docker compose up -d",
            "Tab must complete autosuggestion"
        );
    });
}

#[test]
fn input_auto_grow_height_increases_with_characters_and_newlines() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

    // Initially 1 line
    let initial_rows = app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.read(cx).current_rows()
    });
    assert_eq!(initial_rows, 1, "Input should start with 1 row");

    // Typing multi-line text increases row count automatically
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("line 1\nline 2\nline 3\nline 4", window, cx);
        });
    });
    cx.draw_frame().expect("draw frame with multi-line text");

    let multiline_rows = app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.read(cx).current_rows()
    });
    assert!(
        multiline_rows >= 4,
        "Input row count must increase as characters/lines increase (auto-grow)"
    );

    // Shift+Enter inserts another line
    cx.update_window_entity(&app_state, |state, window, cx| {
        let ev = gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("shift-enter").unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        state.handle_pane_input_key(pane_id, &ev, window, cx);
    });
    cx.draw_frame().expect("draw frame after shift-enter");

    let after_shift_enter_rows = app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.read(cx).current_rows()
    });
    assert!(
        after_shift_enter_rows >= 5,
        "Shift+Enter must add newline and increase height"
    );
}

#[test]
fn enter_executes_command_and_does_not_insert_newline() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

    // Type command "ls -la"
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("ls -la", window, cx);
        });
        // Simulate physical Enter keydown
        let ev = gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        state.handle_pane_input_key(pane_id, &ev, window, cx);
    });
    wait_for_terminal_submission(&mut cx, &app_state, pane_id);

    // Verify: Enter executed the command, cleared the input field, and did NOT insert newline into input!
    app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        assert_eq!(
            input.read(cx).value().as_str(),
            "",
            "Enter must execute and clear input, not insert a newline"
        );
        assert_eq!(
            pane.command_history.last().map(|s| s.as_str()),
            Some("ls -la")
        );
    });

    // Also test dispatching Enter action directly (e.g. from GPUI action system)
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("pwd", window, cx);
        });
        input.update(cx, |inp, cx| {
            inp.enter(
                &gpui_component::input::Enter { secondary: false },
                window,
                cx,
            );
        });
    });

    // In auto-grow mode, enter() should NOT have inserted \n into the text
    app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        assert!(
            !input.read(cx).value().contains('\n'),
            "Enter action must not insert newline into auto-grow prompt"
        );
    });
}

#[test]
fn typing_shows_ghost_history_suggestion_and_right_accepts_it() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        pane.command_history
            .push("docker compose up -d".to_string());
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("docker comp", window, cx);
        });
    });
    cx.run_until_parked();

    app_state.read_with(&cx, |state, cx| {
        let input = state.projects[0].sessions[0].panes[0]
            .input_state
            .as_ref()
            .unwrap();
        assert_eq!(
            input.read(cx).inline_completion_text(),
            Some("ose up -d"),
            "typing a history prefix must show ghost text"
        );
    });

    cx.update_window_entity(&app_state, |state, window, cx| {
        let ev = gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("right").unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        state.handle_pane_input_key(pane_id, &ev, window, cx);
    });

    app_state.read_with(&cx, |state, cx| {
        let input = state.projects[0].sessions[0].panes[0]
            .input_state
            .as_ref()
            .unwrap();
        assert_eq!(input.read(cx).value().as_str(), "docker compose up -d");
    });
}

#[test]
fn completion_menu_opens_navigates_and_accepts_with_tab() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("cat Cargo", window, cx);
        });
        let ev = gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("tab").unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        state.handle_pane_input_key(pane_id, &ev, window, cx);
    });

    app_state.read_with(&cx, |state, _| {
        let menu = state.projects[0].sessions[0].panes[0]
            .completion_menu
            .as_ref()
            .expect("Tab must open the completion menu");
        assert_eq!(menu.items.len(), 2, "Cargo.lock and Cargo.toml match");
        assert_eq!(menu.selected, 0);
    });

    // Move the highlight down, then accept with Tab.
    cx.update_window_entity(&app_state, |state, window, cx| {
        let ev = gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("down").unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        state.handle_pane_input_key(pane_id, &ev, window, cx);
        let ev = gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("tab").unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        state.handle_pane_input_key(pane_id, &ev, window, cx);
    });

    app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        assert_eq!(input.read(cx).value().as_str(), "cat Cargo.toml");
        assert!(pane.completion_menu.is_none(), "accepting closes the menu");
        assert!(
            pane.command_history.is_empty(),
            "completion must not execute"
        );
    });
}

#[test]
fn right_and_ctrl_c_flow_through_the_action_pipeline() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    // Ghost suggestion set through the deferred change handler, then
    // accepted with Right (bound to MoveRight in the Input context).
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        pane.command_history
            .push("docker compose up -d".to_string());
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("docker comp", window, cx);
            inp.focus(window, cx);
        });
    });
    cx.run_until_parked();
    cx.draw_frame().expect("draw frame with ghost suggestion");
    cx.simulate_keystrokes("right");
    cx.run_until_parked();

    app_state.read_with(&cx, |state, cx| {
        let input = state.projects[0].sessions[0].panes[0]
            .input_state
            .as_ref()
            .unwrap();
        assert_eq!(input.read(cx).value().as_str(), "docker compose up -d");
    });

    // Ctrl+C (Copy in the Input context) must still interrupt and clear.
    cx.simulate_keystrokes("ctrl-c");
    cx.run_until_parked();
    app_state.read_with(&cx, |state, cx| {
        let input = state.projects[0].sessions[0].panes[0]
            .input_state
            .as_ref()
            .unwrap();
        assert_eq!(input.read(cx).value().as_str(), "");
    });
}

#[test]
fn ctrl_r_palette_accepts_with_enter_through_dispatch() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        pane.command_history.push("git status".to_string());
        pane.command_history.push("git push".to_string());
        state.open_history_search(pane_id, window, cx);
    });
    cx.draw_frame().expect("draw palette frame");

    cx.simulate_keystrokes("enter");
    cx.run_until_parked();

    app_state.read_with(&cx, |state, cx| {
        assert!(state.history_search.is_none(), "Enter closes the palette");
        let input = state.projects[0].sessions[0].panes[0]
            .input_state
            .as_ref()
            .unwrap();
        assert_eq!(input.read(cx).value().as_str(), "git push");
    });
}

#[test]
fn history_and_readline_keys_flow_through_the_action_pipeline() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        pane.command_history.push("echo one".to_string());
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| inp.focus(window, cx));
    });
    cx.draw_frame().expect("draw focused frame");

    // Up is bound to `MoveUp` in the Input context; at the first line it
    // must propagate so history recall runs.
    cx.simulate_keystrokes("up");
    cx.run_until_parked();
    app_state.read_with(&cx, |state, cx| {
        let input = state.projects[0].sessions[0].panes[0]
            .input_state
            .as_ref()
            .unwrap();
        assert_eq!(input.read(cx).value().as_str(), "echo one");
    });

    // Ctrl+A is bound to SelectAll in the Input context; in an auto-grow
    // prompt it must end up as readline "start of line".
    cx.update_window_entity(&app_state, |state, window, cx| {
        let input = state.projects[0].sessions[0].panes[0]
            .input_state
            .as_ref()
            .unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("git status", window, cx);
            inp.focus(window, cx);
        });
    });
    cx.draw_frame().expect("draw frame before Ctrl+A");
    cx.simulate_keystrokes("ctrl-a");
    cx.run_until_parked();
    app_state.read_with(&cx, |state, cx| {
        let input = state.projects[0].sessions[0].panes[0]
            .input_state
            .as_ref()
            .unwrap();
        assert_eq!(input.read(cx).cursor(), 0);
    });
}

#[test]
fn enter_key_executes_once_through_the_action_pipeline() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("echo dispatched", window, cx);
            inp.focus(window, cx);
        });
    });
    cx.draw_frame().expect("draw frame with command");

    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);
    wait_for_terminal_submission(&mut cx, &app_state, pane_id);

    app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        assert_eq!(input.read(cx).value().as_str(), "");
        assert_eq!(
            pane.command_history.last().map(String::as_str),
            Some("echo dispatched")
        );
    });
}

#[test]
fn tab_key_dispatches_completion_not_indentation() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        input.update(cx, |inp, cx| {
            inp.set_value("cat Cargo", window, cx);
            inp.focus(window, cx);
        });
    });
    cx.draw_frame().expect("draw frame with input focused");

    // Goes through the real keybinding (`IndentInline` in the Input
    // context) and then bubbles to the wrapper's completion handler.
    cx.simulate_keystrokes("tab");
    cx.run_until_parked();

    app_state.read_with(&cx, |state, cx| {
        let pane = &state.projects[0].sessions[0].panes[0];
        let input = pane.input_state.as_ref().unwrap();
        assert_eq!(
            input.read(cx).value().as_str(),
            "cat Cargo",
            "Tab must not insert indentation into the prompt"
        );
        assert!(
            pane.completion_menu.is_some(),
            "Tab must open the completion menu through the action pipeline"
        );
    });
}

#[test]
fn ctrl_r_history_search_filters_and_inserts() {
    let mut cx = TestAppContext::new();
    let (mut cx, app_state) = open_app(&mut cx);
    cx.draw_frame().expect("draw initial frame");

    let pane_id = app_state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);

    cx.update_window_entity(&app_state, |state, window, cx| {
        let pane = &mut state.projects[0].sessions[0].panes[0];
        pane.command_history.push("cargo test".to_string());
        pane.command_history.push("git status".to_string());
        pane.command_history.push("git push".to_string());
        let ev = gpui::KeyDownEvent {
            keystroke: gpui::Keystroke::parse("ctrl-r").unwrap(),
            is_held: false,
            prefer_character_input: false,
        };
        state.handle_pane_input_key(pane_id, &ev, window, cx);
        assert!(
            state.history_search.is_some(),
            "Ctrl+R opens history search"
        );
    });

    // Type a query into the palette; the deferred handler refreshes it.
    cx.update_window_entity(&app_state, |state, window, cx| {
        let search = state.history_search.as_ref().unwrap();
        let input = search.input.clone();
        input.update(cx, |inp, cx| {
            inp.set_value("git", window, cx);
        });
    });
    cx.run_until_parked();

    app_state.read_with(&cx, |state, _| {
        let search = state.history_search.as_ref().unwrap();
        assert_eq!(search.matches, vec!["git push", "git status"]);
    });

    cx.update_window_entity(&app_state, |state, window, cx| {
        state.accept_history_search(window, cx);
    });

    app_state.read_with(&cx, |state, cx| {
        assert!(
            state.history_search.is_none(),
            "accepting closes the palette"
        );
        let input = state.projects[0].sessions[0].panes[0]
            .input_state
            .as_ref()
            .unwrap();
        assert_eq!(input.read(cx).value().as_str(), "git push");
    });
}

fn process_rss_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}
