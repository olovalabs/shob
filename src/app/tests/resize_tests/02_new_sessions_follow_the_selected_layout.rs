#[test]
fn new_sessions_follow_the_selected_layout() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |s, window, cx| {
        s.switch_agent_layout(window, cx);
        s.create_new_session_in_project(0, window, cx);
        let session = s.projects[0].sessions.last().unwrap();
        let chat = session.panes.iter().find(|pane| pane.one_mode).unwrap();
        assert!(chat.chat.active);
        assert_eq!(s.focused_pane_id, Some(chat.id));
        assert!(
            session
                .panes
                .iter()
                .filter(|pane| !pane.one_mode)
                .all(|pane| !pane.chat.active)
        );
        s.switch_agent_layout(window, cx);
        s.create_new_session_in_project(0, window, cx);
        let session = s.projects[0].sessions.last().unwrap();
        assert_eq!(session.panes.len(), 1);
        assert!(!session.panes[0].one_mode);
        assert!(!session.panes[0].chat.active);
    });
}

#[test]
fn new_panes_start_without_a_model() {
    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    cx.update_window_entity(&state, |s, window, cx| {
        s.create_new_session_in_project(0, window, cx);
    });
    state.read_with(&cx, |s, _| {
        assert_eq!(s.settings.default_agent_model, None);
        let pane = &s.projects[0].sessions.last().unwrap().panes[0];
        assert_eq!(pane.chat.selected_model, None);
    });
}

#[test]
fn selecting_a_model_updates_all_panes_and_new_panes() {
    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    let model = agent_auth::AgentModel {
        provider: "google-antigravity".into(),
        id: "gemini-test".into(),
        backend: agent_auth::ModelBackend::Shob,
    };
    cx.update_window_entity(&state, |s, window, cx| {
        s.create_new_session_in_project(0, window, cx);
        let pane_id = s.projects[0].sessions[0].panes[0].id;
        s.select_agent_model(pane_id, Some(model.clone()), window, cx);
        s.create_new_session_in_project(0, window, cx);
        assert_eq!(
            s.saved_settings().default_agent_model.as_ref(),
            Some(&model)
        );
        assert!(
            s.projects
                .iter()
                .flat_map(|p| &p.sessions)
                .flat_map(|s| &s.panes)
                .all(|pane| pane.chat.selected_model.as_ref() == Some(&model))
        );
    });
}

#[test]
fn agent_composer_question_collects_a_response_and_hides() {
    use crate::agent_interaction::{self, Field, Kind, Response};
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let (request, reply) = agent_interaction::request(
        "Choose an approach".into(),
        None,
        vec![Field {
            label: "Which approach should I use?".into(),
            kind: Kind::Text,
            options: Vec::new(),
            required: true,
            custom: true,
        }],
        false,
    );
    cx.update_window_entity(&state, |s, _, cx| {
        let pane = &mut s.projects[0].sessions[0].panes[0];
        pane.chat.active = true;
        pane.chat.running = true;
        pane.chat.status = SessionStatus::Blocked;
        pane.chat.requests.push_back(request);
        cx.notify();
    });
    cx.draw_frame().unwrap();
    cx.run_until_parked();
    cx.draw_frame().unwrap();
    let request = cx.debug_bounds("agent-request-panel").unwrap();
    let input = cx.debug_bounds("agent-composer-input").unwrap();
    assert!(request.bottom() <= input.top());
    let submit = cx.debug_bounds("agent-request-submit").unwrap().center();
    cx.simulate_click(submit, Modifiers::none());
    assert!(reply.try_recv().is_err(), "required answer is validated");
    cx.draw_frame().unwrap();
    let field = cx.debug_bounds("agent-request-input").unwrap();
    cx.simulate_click(field.center(), Modifiers::none());
    cx.simulate_input("simpler");
    cx.draw_frame().unwrap();
    let submit = cx.debug_bounds("agent-request-submit").unwrap().center();
    cx.simulate_click(submit, Modifiers::none());
    match reply.try_recv() {
        Ok(Response::Submit(answer)) => assert_eq!(answer[0]["value"], "simpler"),
        Ok(_) => panic!("unexpected response"),
        Err(error) => panic!(
            "no response: {error}; validation error visible: {}",
            cx.debug_bounds("agent-request-error").is_some()
        ),
    }
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-request-panel").is_none());
}

#[test]
fn agent_composer_custom_answer_submits_from_question_panel() {
    use crate::agent_interaction::{self, Field, Kind, Response};
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let (request, reply) = agent_interaction::request(
        "Choose an approach".into(),
        None,
        vec![Field {
            label: "What should I inspect?".into(),
            kind: Kind::Single,
            options: vec!["Layout".into(), "Tests".into()],
            required: true,
            custom: true,
        }],
        false,
    );
    cx.update_window_entity(&state, |s, _, cx| {
        let chat = &mut s.projects[0].sessions[0].panes[0].chat;
        chat.active = true;
        chat.running = true;
        chat.requests.push_back(request);
        cx.notify();
    });
    cx.draw_frame().unwrap();
    let field = cx.debug_bounds("agent-request-input").unwrap().center();
    cx.simulate_click(field, Modifiers::none());
    cx.simulate_input("Docs");
    cx.draw_frame().unwrap();
    let submit = cx.debug_bounds("agent-request-submit").unwrap().center();
    cx.simulate_click(submit, Modifiers::none());
    assert!(matches!(reply.try_recv(), Ok(Response::Submit(answer))
            if answer[0]["value"] == "Docs"));
}

#[test]
fn agent_composer_queues_follow_up_without_touching_transcript() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |s, window, cx| {
        let pane = &mut s.projects[0].sessions[0].panes[0];
        let id = pane.id;
        pane.chat.active = true;
        pane.chat.running = true;
        let input = pane.input_state.clone().unwrap();
        input.update(cx, |input, cx| {
            input.set_value("Check the tests next", window, cx)
        });
        s.submit_agent_prompt(id, window, cx);
        let pane = s.pane_ref(id).unwrap();
        assert_eq!(
            pane.chat.follow_ups.front().map(String::as_str),
            Some("Check the tests next")
        );
        assert!(pane.chat.messages.is_empty());
        assert!(input.read(cx).value().is_empty());
        let saved = s.saved_workspace(cx);
        assert_eq!(
            saved.projects[0].sessions[0].panes[0].queued_messages,
            ["Check the tests next"]
        );
    });
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-follow-up-panel").is_some());
}

#[test]
fn agent_checklist_hides_automatically_only_after_all_steps_are_complete() {
    use agent_bridge::AgentEvent;

    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let report = |first: &str, second: &str| AgentEvent::ToolEnd {
        id: Some("checklist".into()),
        name: embedded::task_list::NAME.into(),
        output: serde_json::json!({"tasks": [
            {"text": "Inspect the inputs", "status": first},
            {"text": "Prepare the report", "status": second}
        ]})
        .to_string(),
        failed: false,
    };

    cx.update_window_entity(&state, |s, _, cx| {
        let pane = &mut s.projects[0].sessions[0].panes[0];
        let chat = &mut pane.chat;
        chat.active = true;
        chat.running = true;
        chat.messages.push(Arc::new(ChatMessage::new(
            ChatRole::User,
            "Prepare a report".into(),
        )));
        AppState::apply_task_list(
            pane,
            task_list_update(&report("in_progress", "pending")).unwrap(),
            cx,
        );
        cx.notify();
    });
    cx.draw_frame().unwrap();
    let list = cx.debug_bounds("agent-task-list").unwrap();
    let input = cx.debug_bounds("agent-composer-input").unwrap();
    assert!(list.bottom() <= input.top());
    assert!(cx.debug_bounds("agent-task-running").is_some());
    assert!(cx.debug_bounds("agent-task-pending").is_some());

    cx.update_window_entity(&state, |s, _, cx| {
        let pane = &mut s.projects[0].sessions[0].panes[0];
        AppState::apply_task_list(
            pane,
            task_list_update(&report("completed", "in_progress")).unwrap(),
            cx,
        );
        let chat = &mut pane.chat;
        chat.observe_status(&AgentEvent::Done);
        assert_eq!(chat.messages.len(), 1, "updates do not create tool cards");
        cx.notify();
    });
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-task-list").is_some());
    assert!(cx.debug_bounds("agent-task-completed").is_some());
    assert!(cx.debug_bounds("agent-task-running").is_none());

    cx.background_executor
        .advance_clock(TASK_LIST_COMPLETION_DELAY);
    cx.run_until_parked();
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-task-list").is_some());

    cx.update_window_entity(&state, |s, _, cx| {
        let pane = &mut s.projects[0].sessions[0].panes[0];
        AppState::apply_task_list(
            pane,
            task_list_update(&report("completed", "completed")).unwrap(),
            cx,
        );
        cx.notify();
    });
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-task-completed").is_some());

    cx.background_executor
        .advance_clock(TASK_LIST_COMPLETION_DELAY / 2);
    cx.run_until_parked();
    cx.update_window_entity(&state, |s, _, cx| {
        let pane = &mut s.projects[0].sessions[0].panes[0];
        let complete = task_list_update(&report("completed", "completed")).unwrap();
        assert!(!AppState::apply_task_list(pane, complete, cx));
    });
    cx.background_executor
        .advance_clock(TASK_LIST_COMPLETION_DELAY / 2);
    cx.run_until_parked();
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-task-list").is_none());

    cx.update_window_entity(&state, |s, _, cx| {
        let pane = &mut s.projects[0].sessions[0].panes[0];
        for (first, second) in [
            ("completed", "pending"),
            ("completed", "completed"),
            ("completed", "in_progress"),
        ] {
            AppState::apply_task_list(pane, task_list_update(&report(first, second)).unwrap(), cx);
        }
        cx.notify();
    });
    cx.background_executor
        .advance_clock(TASK_LIST_COMPLETION_DELAY);
    cx.run_until_parked();
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-task-list").is_some());
}

#[test]
fn agent_checklist_scrolls_to_active_step_unless_user_is_browsing() {
    use embedded::task_list::{Task, TaskList, TaskStatus};

    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let report = |active| TaskList {
        tasks: (0..12)
            .map(|index| Task {
                text: format!("Step {}", index + 1),
                status: if index == active {
                    TaskStatus::InProgress
                } else {
                    TaskStatus::Pending
                },
            })
            .collect(),
    };
    let scroll = cx.update_window_entity(&state, |s, _, cx| {
        let pane = &mut s.projects[0].sessions[0].panes[0];
        pane.chat.active = true;
        pane.chat.running = true;
        AppState::apply_task_list(pane, report(0), cx);
        cx.notify();
        pane.chat.checklist.scroll.clone()
    });
    cx.draw_frame().unwrap();
    assert_eq!(scroll.offset().y, px(0.0));

    cx.update_window_entity(&state, |s, _, cx| {
        let pane = &mut s.projects[0].sessions[0].panes[0];
        AppState::apply_task_list(pane, report(10), cx);
        cx.notify();
    });
    cx.draw_frame().unwrap();
    assert!(scroll.offset().y < px(0.0));
    let position = cx.debug_bounds("agent-task-list").unwrap().center();
    cx.simulate_mouse_move(position, None, Modifiers::none());
    cx.draw_frame().unwrap();
    cx.simulate_event(gpui::ScrollEvent {
        position,
        delta: gpui::ScrollDelta::Pixels(point(px(0.0), px(20.0))),
        ..Default::default()
    });
    cx.draw_frame().unwrap();
    let offset = scroll.offset();
    assert!(offset.y < px(0.0));

    cx.update_window_entity(&state, |s, _, cx| {
        let pane = &mut s.projects[0].sessions[0].panes[0];
        assert!(pane.chat.checklist.browsing);
        AppState::apply_task_list(pane, report(0), cx);
        cx.notify();
    });
    cx.draw_frame().unwrap();
    assert_eq!(scroll.offset(), offset);

    cx.simulate_mouse_move(point(px(1.0), px(1.0)), None, Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(scroll.offset().y > offset.y);
    let list = cx.debug_bounds("agent-task-list").unwrap();
    let active = cx.debug_bounds("agent-task-running").unwrap();
    assert!(active.top() >= list.top() && active.bottom() <= list.bottom());
}

#[test]
fn completion_menu_sits_directly_above_composer() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_chat_mode(pane_id, true, window, cx);
        let input = s.pane_ref(pane_id).unwrap().input_state.clone().unwrap();
        input.update(cx, |input, cx| input.set_value("/", window, cx));
        s.on_pane_input_changed(pane_id, window, cx);
    });
    cx.draw_frame().unwrap();
    let menu = cx.debug_bounds("completion-menu").unwrap();
    let bar = cx.debug_bounds("shob-input-bar").unwrap();
    let input = cx.debug_bounds("agent-composer-input").unwrap();
    assert!(
        menu.bottom() <= input.top(),
        "menu {menu:?}, bar {bar:?}, input {input:?}"
    );
    assert!(
        (menu.bottom() - bar.top()).abs() < px(20.0),
        "menu should be adjacent to the composer: menu {menu:?}, bar {bar:?}"
    );
}

#[test]
fn one_view_completion_menu_is_centered_on_composer() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |s, window, cx| {
        s.switch_agent_layout(window, cx);
        let pane_id = s.focused_pane_id.unwrap();
        let input = s.pane_ref(pane_id).unwrap().input_state.clone().unwrap();
        input.update(cx, |input, cx| input.set_value("/", window, cx));
        s.on_pane_input_changed(pane_id, window, cx);
    });
    for width in [1200.0, 640.0] {
        cx.simulate_resize(size(px(width), px(780.0)));
        cx.run_until_parked();
        cx.draw_frame().unwrap();
        let menu = cx.debug_bounds("completion-menu").unwrap();
        let bar = cx.debug_bounds("shob-input-bar").unwrap();
        assert!(
            (menu.center().x - bar.center().x).abs() <= px(1.0),
            "menu {menu:?}, bar {bar:?}"
        );
        assert!(menu.left() >= bar.left() && menu.right() <= bar.right(), "menu {menu:?}, bar {bar:?}");
        if let Some(path) = std::env::var_os("SHOB_BENCH_SCREENSHOT") {
            let path = PathBuf::from(path).with_file_name(format!("shob-completion-{width}.png"));
            cx.update(|window, _| window.render_to_image().unwrap().save(path).unwrap());
        }
    }
}

#[test]
fn completion_menu_closes_on_outside_click() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let pane_id = cx.update_window_entity(&state, |s, window, cx| {
        s.switch_agent_layout(window, cx);
        let pane_id = s.focused_pane_id.unwrap();
        let input = s.pane_ref(pane_id).unwrap().input_state.clone().unwrap();
        input.update(cx, |input, cx| input.set_value("/", window, cx));
        s.on_pane_input_changed(pane_id, window, cx);
        pane_id
    });
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("completion-menu").is_some());
    cx.simulate_click(point(px(10.0), px(100.0)), Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(state.read_with(&cx, |s, _| {
        s.pane_ref(pane_id).unwrap().completion_menu.is_none()
    }));
}

#[test]
fn browser_icon_stays_on_first_line_in_multiline_input() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_chat_mode(pane_id, true, window, cx);
        let input = s.pane_ref(pane_id).unwrap().input_state.clone().unwrap();
        set_input_text(&input, "@browser one line".into(), window, cx);
        s.on_pane_input_changed(pane_id, window, cx);
    });
    cx.draw_frame().unwrap();
    let one_line = cx.debug_bounds("agent-browser-icon").unwrap().top()
        - cx.debug_bounds("agent-composer-input").unwrap().top();
    cx.update_window_entity(&state, |s, window, cx| {
        let input = s.pane_ref(pane_id).unwrap().input_state.clone().unwrap();
        set_input_text(
            &input,
            "@browser first line\n\nsecond line\nthird line".into(),
            window,
            cx,
        );
        s.on_pane_input_changed(pane_id, window, cx);
    });
    cx.draw_frame().unwrap();
    let multiline = cx.debug_bounds("agent-browser-icon").unwrap().top()
        - cx.debug_bounds("agent-composer-input").unwrap().top();
    assert!((one_line - multiline).abs() < px(1.0));
}

#[test]
fn browser_completion_inserts_mention_without_sending() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_chat_mode(pane_id, true, window, cx);
        let input = s.pane_ref(pane_id).unwrap().input_state.clone().unwrap();
        set_input_text(&input, "@bro".into(), window, cx);
        s.on_pane_input_changed(pane_id, window, cx);
        s.accept_completion_at(pane_id, 0, window, cx);
        assert_eq!(input.read(cx).value().as_str(), "@browser ");
        assert!(s.pane_ref(pane_id).unwrap().chat.messages.is_empty());
        s.submit_agent_prompt(pane_id, window, cx);
        assert_eq!(input.read(cx).value().as_str(), "@browser ");
    });
}

#[cfg(not(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64"))))]
#[test]
fn browser_agent_without_native_pane_opens_helper_setup() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.update_window_entity(&state, |s, window, cx| {
        let pane_id = s.projects[0].sessions[0].panes[0].id;
        s.set_chat_mode(pane_id, true, window, cx);
        let input = s.pane_ref(pane_id).unwrap().input_state.clone().unwrap();
        let draft = "@browser open example.com";
        set_input_text(&input, draft.into(), window, cx);
        s.submit_agent_prompt(pane_id, window, cx);
        assert_eq!(input.read(cx).value().as_str(), draft);
        assert!(s.pane_ref(pane_id).unwrap().chat.messages.is_empty());
        assert_eq!(s.active_view, ActiveView::Settings);
        assert_eq!(s.settings_section, SettingsSection::Browser);
    });
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("browser-install-action").is_some());
}

#[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "aarch64")))]
#[test]
fn browser_agent_routes_to_the_visible_thread_without_taking_focus() {
    use crate::browser_automation::Command;

    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.draw_frame().unwrap();
    cx.update_window_entity(&state, |s, window, cx| {
        let owner = s.projects[0].sessions[0].panes[0].id;
        s.settings.agent_layout = true;
        s.set_chat_mode(owner, true, window, cx);
        let pane = s.pane_mut(owner).unwrap();
        pane.chat.running = true;
        let generation = pane.generation.clone();
        let focused = s.focused_pane_id;
        let mut opened = false;
        let open = Command::Open {
            url: "http://localhost:3000/".into(),
            new_tab: false,
        };

        assert!(
            s.browser_tool_target(owner, "stale", &mut opened, &open, window, cx)
                .is_err()
        );
        assert!(s.active_browser_pane().is_none());
        assert!(
            s.browser_tool_target(owner, &generation, &mut opened, &open, window, cx)
                .unwrap()
                .is_some()
        );
        assert_eq!(s.focused_pane_id, focused);
        assert_eq!(s.active_browser_pane().unwrap().preview_owner, Some(owner));

        s.hide_tool_panel(owner, window, cx);
        assert!(
            s.browser_tool_target(owner, &generation, &mut opened, &open, window, cx)
                .is_err()
        );
        assert!(s.active_browser_pane().is_none());
    });
    cx.draw_frame().unwrap();
}

#[test]
fn agent_slash_menu_filters_only_command_input() {
    let names = |input: &str| {
        agent_slash_completions(input)
            .into_iter()
            .map(|item| item.value)
            .collect::<Vec<_>>()
    };
    assert_eq!(names("/").len(), AGENT_SLASH_COMMANDS.len());
    assert_eq!(names("/mo"), vec!["/model"]);
    assert_eq!(names("/TH"), vec!["/thinking"]);
    assert!(names("/thinking high").is_empty());
    assert!(names("explain /model").is_empty());
    assert!(names("/unknown").is_empty());
    assert_eq!(names("@bro"), vec!["@browser"]);
    assert!(names("@browser open example.com").is_empty());
}

#[test]
fn agent_title_from_prompt_names_the_conversation() {
    assert_eq!(
        agent_title_from_prompt("  Refactor   the auth module\nand add tests"),
        Some("Refactor the auth module".to_string())
    );
    // Slash commands are not conversation content.
    assert_eq!(agent_title_from_prompt("/model"), None);
    assert_eq!(agent_title_from_prompt("   \n\t "), None);

    let long = "a".repeat(200);
    let title = agent_title_from_prompt(&long).unwrap();
    assert!(title.ends_with('…'));
    assert_eq!(title.chars().count(), 65);
}

#[test]
fn permission_mode_applies_to_all_panes_and_new_splits() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    state.update(&mut cx, |s, cx| {
        s.pane_mut(1).unwrap().chat.active = true;
        cx.notify();
    });
    cx.draw_frame().unwrap();
    let button = cx.debug_bounds("agent-permissions").unwrap();
    cx.simulate_click(button.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    let menu = cx.debug_bounds("agent-permission-menu").unwrap();
    assert!(menu.bottom() <= button.top());
    let auto = cx.debug_bounds("agent-permission-option-1").unwrap();
    cx.simulate_click(auto.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-permission-menu").is_none());
    state.read_with(&cx, |s, _| {
        assert_eq!(
            s.settings.permission_mode,
            agent_bridge::PermissionMode::Auto
        );
    });
    cx.update_window_entity(&state, |s, window, cx| {
        s.split_pane(1, gpui::Axis::Horizontal, window, cx);
        for mode in [
            agent_bridge::PermissionMode::Full,
            agent_bridge::PermissionMode::Auto,
            agent_bridge::PermissionMode::Ask,
        ] {
            s.set_permission_mode(mode, cx);
            assert_eq!(s.saved_settings().permission_mode, mode);
            assert!(
                s.projects
                    .iter()
                    .flat_map(|p| &p.sessions)
                    .flat_map(|s| &s.panes)
                    .all(|p| p.chat.permission_mode == mode)
            );
        }
        s.set_permission_mode(agent_bridge::PermissionMode::Full, cx);
        s.split_pane(1, gpui::Axis::Vertical, window, cx);
        assert!(
            s.projects[0].sessions[0]
                .panes
                .iter()
                .all(|p| p.chat.permission_mode == agent_bridge::PermissionMode::Full)
        );
        s.create_new_session_in_project(0, window, cx);
        assert_eq!(
            s.projects[0].sessions.last().unwrap().panes[0]
                .chat
                .permission_mode,
            agent_bridge::PermissionMode::Full
        );
    });
}

#[test]
fn native_agent_sidebar_lifecycle_ignores_hidden_shell() {
    use agent_bridge::AgentEvent;

    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.draw_frame().unwrap();
    state.update(&mut cx, |s, cx| {
        let pane = &mut s.projects[0].sessions[0].panes[0];
        pane.chat.active = true;
        pane.chat.running = true;
        pane.chat.status = SessionStatus::Working;
        // A stale shell failure must not mask native chat activity.
        pane.native_status = Some(SessionStatus::Error);
        s.poll_sessions_status_with(&HashMap::new(), cx);
        assert_eq!(s.projects[0].sessions[0].status, SessionStatus::Working);
        assert_eq!(
            s.projects[0].sessions[0].panes[0].detection.evidence,
            status::EvidenceSource::AgentLifecycle,
        );
    });
    cx.draw_frame().unwrap();

    // Switching to the terminal must not hide a still-running agent.
    state.update(&mut cx, |s, cx| {
        s.projects[0].sessions[0].panes[0].chat.active = false;
        s.poll_sessions_status_with(&HashMap::new(), cx);
        assert_eq!(s.projects[0].sessions[0].status, SessionStatus::Working);
        s.window_active = false;
        let pane = &mut s.projects[0].sessions[0].panes[0];
        pane.chat.active = true;
        pane.chat.observe_status(&AgentEvent::Done);
        s.poll_sessions_status_with(&HashMap::new(), cx);
        s.poll_sessions_status_with(&HashMap::new(), cx);
        assert_eq!(s.projects[0].sessions[0].status, SessionStatus::Done);
    });
    cx.draw_frame().unwrap();

    // Looking acknowledges completion permanently, including after leaving.
    state.update(&mut cx, |s, cx| {
        s.window_active = true;
        s.poll_sessions_status_with(&HashMap::new(), cx);
        assert_eq!(s.projects[0].sessions[0].status, SessionStatus::Active);
        s.window_active = false;
        s.poll_sessions_status_with(&HashMap::new(), cx);
        assert_ne!(s.projects[0].sessions[0].status, SessionStatus::Done);

        let pane = &mut s.projects[0].sessions[0].panes[0];
        pane.chat.running = true;
        pane.chat.status = SessionStatus::Working;
        pane.chat
            .observe_status(&AgentEvent::Error("Provider failed".into()));
        pane.chat.observe_status(&AgentEvent::Done);
        s.poll_sessions_status_with(&HashMap::new(), cx);
        assert_eq!(s.projects[0].sessions[0].status, SessionStatus::Error);
    });
    cx.draw_frame().unwrap();

    // New work clears a prior failure; closing mid-turn fails the new turn.
    state.update(&mut cx, |s, cx| {
        let pane = &mut s.projects[0].sessions[0].panes[0];
        pane.chat.running = true;
        pane.chat.status = SessionStatus::Working;
        s.poll_sessions_status_with(&HashMap::new(), cx);
        assert_eq!(s.projects[0].sessions[0].status, SessionStatus::Working);
        s.projects[0].sessions[0].panes[0]
            .chat
            .observe_status(&AgentEvent::Closed);
        s.poll_sessions_status_with(&HashMap::new(), cx);
        assert_eq!(s.projects[0].sessions[0].status, SessionStatus::Error);
    });
}

#[cfg(unix)]
#[test]
fn worker_reports_do_not_change_parent_lifecycle() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    state.update(&mut cx, |s, cx| {
        let pane = &s.projects[0].sessions[0].panes[0];
        let mut event = status_bus::StatusEvent {
            version: 1,
            app_id: STATUS_BUS.as_ref().unwrap().app_id.clone(),
            pane_id: pane.id as u64,
            pane_generation: pane.generation.clone(),
            reporter: "test-agent".into(),
            reporter_generation: "parent-generation".into(),
            sequence: 1,
            session_id: None,
            turn_id: None,
            event: "working".into(),
            message: None,
            approval_id: None,
            agent_id: None,
            parent: None,
        };
        let mut worker = event.clone();
        worker.agent_id = Some("child".into());
        worker.reporter_generation = "worker-generation".into();
        worker.event = "register".into();
        s.apply_status_events(vec![worker], cx);
        let pane = &s.projects[0].sessions[0].panes[0];
        assert!(!pane.is_agent);
        assert_eq!(pane.status, SessionStatus::Idle);
        s.apply_status_events(vec![event.clone()], cx);
        event.sequence = 2;
        event.event = "done".into();
        s.apply_status_events(vec![event], cx);
        assert_eq!(
            s.projects[0].sessions[0].panes[0].native_status,
            Some(SessionStatus::Done)
        );
    });
}

#[test]
fn agent_title_labels_the_sidebar_session() {
    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, window, cx| {
        s.settings.agent_layout = true;
        s.focus_one_chat(pane_id, window, cx);
        let chat_id = s.focused_pane_id.unwrap();
        assert_ne!(chat_id, pane_id);
        assert!(s.sidebar_thread_groups().active.is_empty());
        assert!(s.sidebar_search_thread_refs("").is_empty());
        s.pane_mut(chat_id)
            .unwrap()
            .chat
            .set_prompt_title("Refactor the auth module".into());
        let foregrounds: HashMap<u32, Arc<[String]>> = HashMap::new();
        s.poll_sessions_status_with(&foregrounds, cx);
        assert_eq!(s.sidebar_thread_groups().active.len(), 1);
        assert_eq!(s.sidebar_search_thread_refs("Refactor").len(), 1);
        s.focused_pane_id = None;
        s.poll_sessions_status_with(&foregrounds, cx);
        assert_eq!(
            s.projects[0].sessions[0].title.as_str(),
            "Refactor the auth module"
        );
    });
    state.read_with(&cx, |s, _| {
        let session = &s.projects[0].sessions[0];
        assert_eq!(session.title.as_str(), "Refactor the auth module");
        assert_eq!(session.display_title.as_str(), "Refactor the auth module");
    });
}

#[test]
fn agent_reported_title_wins_over_the_prompt_title() {
    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_chat_mode(pane_id, true, window, cx);
        let chat = &mut s.pane_mut(pane_id).unwrap().chat;
        chat.set_prompt_title("Refactor the auth module".into());
        chat.set_agent_title("🔐 AUTH: Add token refresh".into());
        s.sync_session_title(pane_id);
    });
    state.read_with(&cx, |s, cx| {
        let title = "🔐 AUTH: Add token refresh";
        let session = &s.projects[0].sessions[0];
        assert_eq!(session.display_title.as_str(), title);
        let saved = s.saved_workspace(cx);
        assert_eq!(
            saved.projects[0].sessions[0].panes[0]
                .chat
                .agent_title
                .as_deref(),
            Some(title)
        );
    });
}

#[test]
fn switching_models_preserves_the_pane_conversation() {
    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
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
        s.set_chat_mode(pane_id, true, window, cx);
        let pane = s.pane_mut(pane_id).unwrap();
        pane.chat.selected_model = Some(deepseek);
        pane.chat.running = true;
        let active_bridge = pane.chat.bridge.as_ref().map(ModelBridge::trace_id);
        pane.chat.set_agent_title("Existing conversation".into());
        pane.chat.messages.push(Arc::new(ChatMessage::new(
            ChatRole::User,
            "Remember this context".into(),
        )));
        let input = pane.input_state.as_ref().unwrap().clone();
        input.update(cx, |state, cx| state.set_value("Unsent draft", window, cx));
        s.select_agent_model(pane_id, Some(grok.clone()), window, cx);
        let pane = s.pane_ref(pane_id).unwrap();
        assert_eq!(pane.chat.selected_model.as_ref(), Some(&grok));
        assert!(pane.chat.running);
        assert_eq!(
            pane.chat.bridge.as_ref().map(ModelBridge::trace_id),
            active_bridge
        );
        assert_eq!(
            pane.chat.conversation_title(),
            Some("Existing conversation")
        );
        assert_eq!(
            pane.chat.visible_messages()[0].text,
            "Remember this context"
        );
        assert_eq!(input.read(cx).value().as_str(), "Unsent draft");
        let handoff = pane.chat.context_prompt("Continue").unwrap();
        assert!(handoff.contains("Remember this context"));
        assert!(handoff.ends_with("Current user request:\nContinue"));
    });
}

#[test]
fn plain_shell_sessions_keep_their_osc_title() {
    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    cx.update_window_entity(&state, |s, _, cx| {
        s.projects[0].sessions[0].panes[0]
            .terminal
            .update(cx, |term, _| {
                term.set_title(Some("~/project - fish".into()));
            });
        s.projects[0].sessions[0].panes[0].title = "~/project - fish".into();
        s.projects[0].sessions[0].panes[0].display_title = "~/project - fish".into();
        let foregrounds: HashMap<u32, Arc<[String]>> = HashMap::new();
        s.poll_sessions_status_with(&foregrounds, cx);
    });
    state.read_with(&cx, |s, _| {
        assert_eq!(s.projects[0].sessions[0].title.as_str(), "~/project - fish");
    });
}

#[test]
fn agent_default_screen_shows_for_an_empty_chat() {
    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    let model = agent_auth::AgentModel {
        provider: "google-antigravity".into(),
        id: "gemini-default".into(),
        backend: agent_auth::ModelBackend::Shob,
    };
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_chat_mode(pane_id, true, window, cx);
        // Set the model after entering agent mode so no bridge is warmed.
        s.pane_mut(pane_id).unwrap().chat.selected_model = Some(model.clone());
    });
    cx.draw_frame().expect("draw agent default screen");
    assert!(
        cx.debug_bounds("agent-default-screen").is_some(),
        "an empty agent pane shows the default screen"
    );
    state.read_with(&cx, |s, _| {
        let pane = s.pane_ref(pane_id).unwrap();
        assert_eq!(pane.chat.selected_model.as_ref(), Some(&model));
        assert!(pane.chat.visible_messages().is_empty());
    });
}
