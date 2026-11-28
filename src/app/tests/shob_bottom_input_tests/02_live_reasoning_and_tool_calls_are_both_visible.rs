#[test]
fn live_reasoning_and_tool_calls_are_both_visible() {
    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_chat_mode(pane_id, true, window, cx);
        let pane = s.pane_mut(pane_id).unwrap();
        pane.chat.running = true;
        let reasoning = (0..20)
            .map(|line| format!("reasoning line {line}\n"))
            .collect::<String>();
        push_chat_event(
            &mut pane.chat.messages,
            agent_bridge::AgentEvent::Thinking(reasoning),
        );
        push_chat_event(
            &mut pane.chat.messages,
            agent_bridge::AgentEvent::ToolStart {
                id: Some("t1".into()),
                name: "read".into(),
                input: r#"{"path":"src/main.rs"}"#.into(),
            },
        );
        push_chat_event(
            &mut pane.chat.messages,
            agent_bridge::AgentEvent::ToolEnd {
                id: Some("t1".into()),
                name: "read".into(),
                output: "file contents".into(),
                failed: false,
            },
        );
        push_chat_event(
            &mut pane.chat.messages,
            agent_bridge::AgentEvent::ToolStart {
                id: Some("t2".into()),
                name: "bash".into(),
                input: r#"{"command":"cargo check"}"#.into(),
            },
        );
    });
    cx.draw_frame().expect("draw reasoning header");
    let reasoning = cx.debug_bounds("thinking-row-0").expect("reasoning header");
    cx.simulate_click(
        reasoning.origin + point(px(20.0), px(10.0)),
        Modifiers::default(),
    );
    cx.draw_frame()
        .expect("expand reasoning during tool execution");
    let body = cx
        .debug_bounds("thinking-body-0")
        .expect("expanded reasoning stays visible during tool execution");
    assert!(
        body.size.height > px(40.0),
        "the full reasoning body is laid out, not a one-line preview: {:?}",
        body.size.height
    );
    assert!(cx.debug_bounds("thinking-row-0").is_some());
    let read = cx.debug_bounds("tool-row-1").expect("read row");
    assert!(cx.debug_bounds("tool-row-2").is_some(), "running shell row");

    cx.simulate_click(
        read.origin + point(px(20.0), px(10.0)),
        Modifiers::default(),
    );
    cx.draw_frame().expect("draw expanded read");
    let read = cx.debug_bounds("tool-row-1").expect("expanded read header");
    let shell = cx
        .debug_bounds("tool-row-2")
        .expect("shell row after expanded read");
    assert!(shell.origin.y - read.origin.y > px(24.0));

    cx.update_window_entity(&state, |s, _, cx| {
        let pane = s.pane_mut(pane_id).unwrap();
        push_chat_event(
            &mut pane.chat.messages,
            agent_bridge::AgentEvent::ToolEnd {
                id: Some("t2".into()),
                name: "bash".into(),
                output: "finished".into(),
                failed: false,
            },
        );
        push_chat_event(
            &mut pane.chat.messages,
            agent_bridge::AgentEvent::Text("All checks passed.".into()),
        );
        pane.chat.mark_active_dirty();
        cx.notify();
    });
    cx.draw_frame().expect("draw completed section");
    let group = cx
        .debug_bounds("tool-group-0")
        .expect("folded activity section");
    assert!(cx.debug_bounds("tool-row-1").is_none());
    assert!(cx.debug_bounds("tool-row-2").is_none());

    cx.simulate_click(
        group.origin + point(px(20.0), px(10.0)),
        Modifiers::default(),
    );
    cx.draw_frame().expect("draw reopened section");
    assert!(cx.debug_bounds("tool-row-1").is_some());
    assert!(cx.debug_bounds("tool-row-2").is_some());
}

#[test]
fn reasoning_starts_folded_and_can_be_expanded_live_and_after_completion() {
    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_chat_mode(pane_id, true, window, cx);
        let pane = s.pane_mut(pane_id).unwrap();
        pane.chat.running = true;
        let reasoning = (0..120)
            .map(|line| format!("Reasoning paragraph {line}.\n\n"))
            .collect::<String>();
        push_chat_event(
            &mut pane.chat.messages,
            agent_bridge::AgentEvent::Thinking(reasoning),
        );
    });
    cx.draw_frame().expect("draw live reasoning header");
    assert!(cx.debug_bounds("thinking-body-0").is_none());
    let row = cx.debug_bounds("thinking-row-0").expect("thinking row");
    cx.simulate_click(row.origin + point(px(20.0), px(10.0)), Modifiers::default());
    cx.draw_frame().expect("expand live reasoning");
    cx.run_until_parked();
    cx.draw_frame().expect("draw parsed reasoning");
    assert!(cx.debug_bounds("thinking-body-0").is_some());
    assert_eq!(cx.debug_bounds("thinking-row-0").unwrap().top(), row.top());
    state.read_with(&cx, |s, _| {
        let list = &s.pane_ref(pane_id).unwrap().chat.list_state;
        assert!(!list.is_following_tail(), "opening tall output must not jump to its end");
    });
    cx.update_window_entity(&state, |s, _, cx| {
        let pane = s.pane_mut(pane_id).unwrap();
        let event = agent_bridge::AgentEvent::Thinking("another reasoning line\n".into());
        pane.chat.observe_status(&event);
        push_chat_event(&mut pane.chat.messages, event);
        assert!(pane.chat.messages[0].expanded);
        cx.notify();
    });
    cx.draw_frame().expect("draw expanded reasoning delta");
    assert!(cx.debug_bounds("thinking-body-0").is_some());
    cx.update_window_entity(&state, |s, _, cx| {
        let pane = s.pane_mut(pane_id).unwrap();
        let event = agent_bridge::AgentEvent::Text("Answer".into());
        pane.chat.observe_status(&event);
        push_chat_event(&mut pane.chat.messages, event);
        cx.notify();
    });
    cx.draw_frame().expect("draw completed reasoning header");
    assert!(cx.debug_bounds("thinking-body-0").is_none());
    let activity = cx.debug_bounds("tool-group-0").expect("completed activity");
    cx.simulate_click(activity.center(), Modifiers::default());
    cx.draw_frame().expect("open completed activity");
    let row = cx.debug_bounds("thinking-row-0").expect("thinking row");
    cx.simulate_click(row.origin + point(px(20.0), px(10.0)), Modifiers::default());
    cx.draw_frame().expect("expand completed reasoning");
    let body = cx
        .debug_bounds("thinking-body-0")
        .expect("completed reasoning is visible");
    assert!(
        body.size.height > px(40.0),
        "the visible body contains multiple lines: {:?}",
        body.size.height
    );
    let row = cx.debug_bounds("thinking-row-0").expect("thinking row");
    cx.simulate_click(row.origin + point(px(20.0), px(10.0)), Modifiers::default());
    cx.draw_frame().expect("draw folded reasoning");
    assert!(cx.debug_bounds("thinking-body-0").is_none());
}

#[test]
fn saved_antigravity_flash_uses_native_gemini() {
    let mut chat = PaneChat {
        selected_model: Some(agent_auth::AgentModel {
            provider: "google-antigravity".into(),
            id: "gemini-3.8-flash".into(),
            backend: agent_auth::ModelBackend::Shob,
        }),
        ..PaneChat::default()
    };
    assert_eq!(chat.mode_thinking_level(), "low");
    assert_eq!(
        agent_model_selector(chat.selected_model.as_ref().unwrap()),
        "google-antigravity/gemini-3.8-flash"
    );
    chat.fast_mode = false;
    assert_eq!(chat.mode_thinking_level(), "high");

    chat.fast_mode = true;
    chat.selected_model.as_mut().unwrap().provider = "google".into();
    assert_eq!(chat.mode_thinking_level(), "low");
}

#[test]
fn reasoning_preferences_follow_the_selected_model() {
    let mut chat = PaneChat::default();
    let model = |id: &str| agent_auth::AgentModel {
        provider: "test".into(),
        id: id.into(),
        backend: agent_auth::ModelBackend::Embedded,
    };
    chat.thinking_by_model
        .insert("test/first".into(), "medium".into());
    chat.thinking_by_model
        .insert("test/second".into(), "high".into());
    chat.selected_model = Some(model("first"));
    assert_eq!(chat.mode_thinking_level(), "medium");
    chat.selected_model = Some(model("second"));
    assert_eq!(chat.mode_thinking_level(), "high");
    chat.selected_model = Some(model("third"));
    assert_eq!(chat.mode_thinking_level(), "low");
}

#[test]
fn first_response_provider_is_recorded_once_from_the_running_model() {
    load_provider_icons();
    let model = |provider: &str| agent_auth::AgentModel {
        provider: provider.into(),
        id: "model".into(),
        backend: agent_auth::ModelBackend::Shob,
    };
    let mut chat = PaneChat {
        bridge_model: Some(model("anthropic")),
        selected_model: Some(model("openai")),
        ..Default::default()
    };
    assert!(
        !chat
            .record_first_response_provider(&agent_bridge::AgentEvent::Thinking("thinking".into()))
    );
    assert!(!chat.record_first_response_provider(&agent_bridge::AgentEvent::Text(String::new())));
    assert!(chat.record_first_response_provider(&agent_bridge::AgentEvent::Text("Hello".into())));
    chat.bridge_model = Some(model("openai"));
    assert!(!chat.record_first_response_provider(&agent_bridge::AgentEvent::Text("Next".into())));
    assert_eq!(chat.first_response_provider.as_deref(), Some("anthropic"));
    assert_eq!(
        provider_icon_path(chat.first_response_provider.as_deref().unwrap()).as_ref(),
        "icons/provider/anthropic.svg"
    );
}

#[test]
fn reasoning_selection_is_saved_and_rejects_unsupported_levels() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, _, cx| {
        s.pane_mut(id).unwrap().chat.selected_model = Some(agent_auth::AgentModel {
            provider: "google-antigravity".into(),
            id: "gemini-3.8-flash".into(),
            backend: agent_auth::ModelBackend::Embedded,
        });
        s.set_reasoning_effort(id, "medium", cx);
        s.set_reasoning_effort(id, "unsupported", cx);
        assert_eq!(s.pane_ref(id).unwrap().chat.mode_thinking_level(), "medium");
        assert_eq!(
            s.saved_workspace(cx).projects[0].sessions[0].panes[0]
                .chat
                .thinking_by_model
                .get("google-antigravity/gemini-3.8-flash")
                .map(String::as_str),
            Some("medium")
        );
    });
}

#[test]
fn explicit_effort_preserves_model_and_fast_deep_clears_override() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.draw_frame().unwrap();
    let id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    let model = agent_auth::AgentModel {
        provider: "google".into(),
        id: "gemini-3.8-flash".into(),
        backend: agent_auth::ModelBackend::Embedded,
    };
    cx.update_window_entity(&state, |s, _, _| {
        let chat = &mut s.pane_mut(id).unwrap().chat;
        chat.active = true;
        chat.selected_model = Some(model.clone());
    });
    for effort in ["auto", "inherit", "low", "medium", "high", "xhigh", "max"] {
        cx.update_window_entity(&state, |s, window, cx| {
            let input = s.pane_ref(id).unwrap().input_state.clone().unwrap();
            input.update(cx, |input, cx| {
                input.set_value(format!("/thinking {effort}"), window, cx)
            });
            s.submit_agent_prompt(id, window, cx);
        });
        state.read_with(&cx, |s, cx| {
            let chat = &s.pane_ref(id).unwrap().chat;
            assert_eq!(chat.selected_model.as_ref(), Some(&model));
            assert_eq!(chat.thinking_override.as_deref(), Some(effort));
            assert_eq!(chat.mode_thinking_level(), effort);
            assert_eq!(
                s.saved_workspace(cx).projects[0].sessions[0].panes[0]
                    .chat
                    .thinking_override
                    .as_deref(),
                Some(effort)
            );
        });
    }
    cx.update_window_entity(&state, |s, window, cx| {
        let input = s.pane_ref(id).unwrap().input_state.clone().unwrap();
        input.update(cx, |input, cx| {
            input.set_value("/thinking invalid", window, cx)
        });
        s.submit_agent_prompt(id, window, cx);
        assert_eq!(
            s.pane_ref(id).unwrap().chat.thinking_override.as_deref(),
            Some("max")
        );
        s.toggle_fast_mode(id, cx);
        assert!(s.pane_ref(id).unwrap().chat.thinking_override.is_none());
        assert_eq!(
            s.pane_ref(id).unwrap().chat.selected_model.as_ref(),
            Some(&model)
        );
        s.pane_mut(id).unwrap().chat.selected_model = None;
        input.update(cx, |input, cx| {
            input.set_value("/thinking high", window, cx)
        });
        s.submit_agent_prompt(id, window, cx);
        assert!(s.pane_ref(id).unwrap().chat.thinking_override.is_none());
    });
}

#[test]
fn agent_dynamic_autoscroll_and_user_lock() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_theme_preference(ThemePreference::Dark, Some(window), cx);
        s.set_chat_mode(id, true, window, cx);
        let pane = s.pane_mut(id).unwrap();
        assert!(pane.chat.list_state.is_following_tail());
        // Add several messages so the container has scrollable content
        for i in 0..15 {
            pane.chat.messages.push(Arc::new(ChatMessage::new(
                if i % 2 == 0 {
                    ChatRole::User
                } else {
                    ChatRole::Assistant
                },
                format!("Message {i} with several lines of text to ensure vertical scrollability."),
            )));
        }
    });
    cx.draw_frame()
        .expect("draw chat frame with autoscroll pinned");
    state.read_with(&cx, |s, _| {
        let pane = s.pane_ref(id).unwrap();
        assert_eq!(
            pane.chat.list_state.is_scrolled_to_end(),
            Some(true),
            "autoscroll must follow the end of the transcript"
        );
    });

    // Append more content while autoscroll is true
    cx.update_window_entity(&state, |s, _, cx| {
        let pane = s.pane_mut(id).unwrap();
        for i in 15..25 {
            pane.chat.messages.push(Arc::new(ChatMessage::new(
                ChatRole::Assistant,
                format!("New streamed chunk {i} making the conversation much taller!"),
            )));
        }
        cx.notify();
    });
    cx.draw_frame()
        .expect("draw frame after new streamed chunks");
    state.read_with(&cx, |s, _| {
        let pane = s.pane_ref(id).unwrap();
        assert_eq!(
            pane.chat.list_state.is_scrolled_to_end(),
            Some(true),
            "autoscroll must follow growing content to the new bottom"
        );
    });
    // A real scroll gesture must release following, not just a state change.
    let bounds = cx.debug_bounds("agent-chat").expect("chat viewport");
    cx.simulate_event(gpui::ScrollEvent {
        position: bounds.center(),
        delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(300.0))),
        modifiers: Default::default(),
        touch_phase: gpui::TouchPhase::Moved,
        source: gpui::ScrollSource::Pointer,
    });
    cx.draw_frame()
        .expect("draw chat frame with autoscroll unpinned");
    state.read_with(&cx, |s, _| {
        let pane = s.pane_ref(id).unwrap();
        assert!(!pane.chat.list_state.is_following_tail());
    });

    // Streaming must preserve the reader's position until they return to the end.
    let anchor = state.read_with(&cx, |s, _| {
        s.pane_ref(id).unwrap().chat.list_state.logical_scroll_top()
    });
    cx.update_window_entity(&state, |s, _, cx| {
        let pane = s.pane_mut(id).unwrap();
        pane.chat.messages.push(Arc::new(ChatMessage::new(
            ChatRole::Assistant,
            "More output while reading history".into(),
        )));
        cx.notify();
    });
    cx.draw_frame().unwrap();
    state.read_with(&cx, |s, _| {
        let chat = &s.pane_ref(id).unwrap().chat;
        assert!(!chat.list_state.is_following_tail());
        assert_eq!(chat.list_state.logical_scroll_top(), anchor);
    });

    cx.simulate_event(gpui::ScrollEvent {
        position: bounds.center(),
        delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(-10000.0))),
        modifiers: Default::default(),
        touch_phase: gpui::TouchPhase::Moved,
        source: gpui::ScrollSource::Pointer,
    });
    cx.draw_frame().unwrap();
    cx.run_until_parked();
    cx.draw_frame().unwrap();
    state.read_with(&cx, |s, _| {
        assert!(
            s.pane_ref(id).unwrap().chat.list_state.is_following_tail(),
            "scrolling to the end resumes following"
        );
    });

    // Scrollbar moves must follow the same rules as wheel gestures.
    cx.update_window_entity(&state, |s, _, cx| {
        let list = &s.pane_ref(id).unwrap().chat.list_state;
        list.scrollbar_drag_started();
        list.set_offset_from_scrollbar(gpui::point(px(0.0), px(0.0)));
        list.scrollbar_drag_ended();
        cx.notify();
    });
    cx.draw_frame().unwrap();
    state.read_with(&cx, |s, _| {
        let list = &s.pane_ref(id).unwrap().chat.list_state;
        assert_eq!(list.logical_scroll_top().item_ix, 0);
        assert!(!list.is_following_tail());
    });

    // The jump button restores following, even after content has grown.
    let button = cx
        .debug_bounds("scroll-to-bottom-btn")
        .expect("jump to bottom button");
    cx.simulate_click(button.center(), gpui::Modifiers::default());
    state.read_with(&cx, |s, _| {
        let pane = s.pane_ref(id).unwrap();
        assert!(
            pane.chat.list_state.is_following_tail(),
            "the list must resume following the tail"
        );
    });
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("scroll-to-bottom-btn").is_none());

    cx.update_window_entity(&state, |s, _, cx| {
        let list = &s.pane_ref(id).unwrap().chat.list_state;
        list.scrollbar_drag_started();
        list.set_offset_from_scrollbar(gpui::point(px(0.0), px(0.0)));
        list.set_offset_from_scrollbar(-list.max_offset_for_scrollbar());
        list.scrollbar_drag_ended();
        cx.notify();
    });
    cx.draw_frame().unwrap();
    state.read_with(&cx, |s, _| {
        let list = &s.pane_ref(id).unwrap().chat.list_state;
        assert!(list.is_following_tail(), "the bottom of the track must resume following");
    });
    assert!(cx.debug_bounds("scroll-to-bottom-btn").is_none());
}

#[test]
fn streaming_activity_stays_below_growing_code_block() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, window, cx| {
        window.set_scroll_layers(true);
        s.set_chat_mode(id, true, window, cx);
        let chat = &mut s.pane_mut(id).unwrap().chat;
        chat.running = true;
        chat.messages.clear();
        chat.messages.push(Arc::new(ChatMessage::new(
            ChatRole::User,
            "Show the file tree.".into(),
        )));
        push_chat_event(
            &mut chat.messages,
            agent_bridge::AgentEvent::Text("```text\nshob/\n".into()),
        );
        cx.notify();
    });
    cx.draw_commands();
    cx.run_until_parked();
    cx.draw_commands();

    for delta in [
        "├── file.rs\n".repeat(5),
        "├── file.rs\n".repeat(12),
        "├── file.rs\n".repeat(23),
        "└── Cargo.toml\n```\n\nThis is the project root.".into(),
    ] {
        cx.update_window_entity(&state, |s, _, cx| {
            let chat = &mut s.pane_mut(id).unwrap().chat;
            let event = agent_bridge::AgentEvent::Text(delta);
            chat.observe_status(&event);
            push_chat_event(&mut chat.messages, event);
            chat.notify_content(cx);
        });
        for _ in 0..2 {
            cx.draw_commands();
            if let (Some(message), Some(activity)) = (
                cx.debug_bounds("chat-message-1"),
                cx.debug_bounds("agent-activity"),
            ) {
                assert!(
                    activity.top() >= message.bottom(),
                    "activity {activity:?} overlaps the streaming response {message:?}",
                );
            }
            cx.run_until_parked();
        }
    }

    cx.draw_commands();
    let activity = cx.debug_bounds("agent-activity").expect("activity at the tail");
    let message = cx.debug_bounds("chat-message-1").expect("streaming response");
    assert!(activity.top() >= message.bottom());
    state.read_with(&cx, |s, _| {
        let list = &s.pane_ref(id).unwrap().chat.list_state;
        assert_eq!(list.item_count(), 2, "the response and activity share a list item");
        assert!(list.is_following_tail());
    });

    cx.update_window_entity(&state, |s, _, cx| {
        let chat = &mut s.pane_mut(id).unwrap().chat;
        let event = agent_bridge::AgentEvent::Done;
        chat.observe_status(&event);
        push_chat_event(&mut chat.messages, event);
        chat.notify_content(cx);
    });
    cx.draw_commands();
    cx.run_until_parked();
    cx.draw_commands();
    assert!(cx.debug_bounds("agent-activity").is_none());
}

#[test]
fn streaming_remeasure_preserves_offset_inside_message() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_chat_mode(id, true, window, cx);
        let chat = &mut s.pane_mut(id).unwrap().chat;
        chat.messages.clear();
        chat.running = true;
        push_chat_event(
            &mut chat.messages,
            agent_bridge::AgentEvent::Text("A long paragraph of streaming output.\n\n".repeat(100)),
        );
        cx.notify();
    });
    cx.draw_frame().unwrap();
    cx.run_until_parked();
    cx.draw_frame().unwrap();
    cx.draw_frame().unwrap();
    cx.update_window_entity(&state, |s, _, cx| {
        let chat = &mut s.pane_mut(id).unwrap().chat;
        chat.list_state.scroll_to(gpui::ListOffset {
            item_ix: 0,
            offset_in_item: px(200.0),
        });
        cx.notify();
    });
    cx.draw_frame().unwrap();
    let anchor = state.read_with(&cx, |s, _| {
        s.pane_ref(id).unwrap().chat.list_state.logical_scroll_top()
    });
    assert_eq!(anchor.item_ix, 0);
    assert!(anchor.offset_in_item > px(0.0));
    cx.update_window_entity(&state, |s, _, cx| {
        let chat = &mut s.pane_mut(id).unwrap().chat;
        push_chat_event(
            &mut chat.messages,
            agent_bridge::AgentEvent::Text("More streaming output".into()),
        );
        chat.mark_active_dirty();
        cx.notify();
    });
    cx.draw_frame().unwrap();
    state.read_with(&cx, |s, _| {
        let chat = &s.pane_ref(id).unwrap().chat;
        assert!(!chat.list_state.is_following_tail());
        assert_eq!(chat.list_state.logical_scroll_top(), anchor);
    });
}

#[test]
fn model_selection_does_not_reauthenticate_ready_provider() {
    use gpui::Modifiers;
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    let model = agent_auth::AgentModel {
        provider: "google-antigravity".into(),
        id: "gemini-example".into(),
        backend: agent_auth::ModelBackend::Shob,
    };
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_chat_mode(id, true, window, cx);
        let input = s.pane_ref(id).unwrap().input_state.clone().unwrap();
        input.update(cx, |input, cx| input.set_value("/model", window, cx));
        s.execute_pane_input(id, window, cx);
        s.agent_catalog.data.providers = vec![agent_auth::Provider {
            name: None,
            custom: false,
            id: "google-antigravity".into(),
            ready: true,
            source: Some("Shob credential".into()),
        }];
        s.agent_catalog.data.models = vec![model.clone()];
    });
    cx.draw_frame().unwrap();
    state.read_with(&cx, |s, _| {
        let auth = s.pane_ref(id).unwrap().chat.auth.as_ref().unwrap();
        assert!(matches!(auth.page, AgentDialogPage::Models));
        assert!(auth.login.is_none());
    });
    assert!(
        cx.debug_bounds("auth-provider-0").is_none(),
        "model page must not show login rows"
    );
    let bounds = cx.debug_bounds("agent-more-providers").unwrap();
    let center = point(
        bounds.origin.x + bounds.size.width / 2.0,
        bounds.origin.y + bounds.size.height / 2.0,
    );
    cx.simulate_mouse_move(center, None, Modifiers::default());
    cx.simulate_click(center, Modifiers::default());
    cx.draw_frame().unwrap();
    let bounds = cx.debug_bounds("auth-provider-0").unwrap();
    let center = point(
        bounds.origin.x + px(10.0),
        bounds.origin.y + bounds.size.height / 2.0,
    );
    cx.simulate_mouse_move(center, None, Modifiers::default());
    cx.simulate_click(center, Modifiers::default());
    cx.draw_frame().unwrap();
    state.read_with(&cx, |s, _| {
        let auth = s.pane_ref(id).unwrap().chat.auth.as_ref().unwrap();
        assert!(matches!(auth.page, AgentDialogPage::Models));
        assert!(
            auth.login.is_none(),
            "clicking a ready provider must not start a second login"
        );
    });
    let bounds = cx.debug_bounds("agent-model-0").unwrap();
    let center = point(
        bounds.origin.x + px(10.0),
        bounds.origin.y + bounds.size.height / 2.0,
    );
    cx.simulate_mouse_move(center, None, Modifiers::default());
    cx.simulate_click(center, Modifiers::default());
    state.read_with(&cx, |s, _| {
        let pane = s.pane_ref(id).unwrap();
        assert!(
            pane.chat.auth.is_some(),
            "the picker stays open to configure reasoning"
        );
        assert_eq!(pane.chat.selected_model.as_ref(), Some(&model));
    });
    cx.draw_frame().unwrap();
    assert!(
        cx.debug_bounds("picker-reasoning-provider-Google Antigravity")
            .is_some()
    );
    cx.simulate_keystrokes("escape");
    state.read_with(&cx, |s, _| {
        assert!(s.pane_ref(id).unwrap().chat.auth.is_none())
    });
}

#[test]
fn custom_provider_form_can_be_opened_and_cancelled_without_saving() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, window, cx| {
        s.open_agent_auth(id, AgentDialogPage::Auth, window, cx);
        let auth = s.pane_mut(id).unwrap().chat.auth.as_mut().unwrap();
        auth.methods_discovery = None;
        auth.selected = None;
        cx.notify();
    });
    cx.draw_frame().unwrap();
    let button = cx.debug_bounds("add-compatible-provider").unwrap();
    cx.simulate_click(button.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    cx.simulate_input("my-gateway");
    state.read_with(&cx, |s, cx| {
        let auth = s.pane_ref(id).unwrap().chat.auth.as_ref().unwrap();
        let fields = auth.custom_provider.as_ref().unwrap();
        assert_eq!(fields[0].read(cx).value().as_str(), "my-gateway");
        assert!(auth.selected.is_none());
        assert!(auth.provider_operation.is_none());
    });
    cx.draw_frame().unwrap();
    let cancel = cx.debug_bounds("cancel-compatible-provider").unwrap();
    cx.simulate_click(cancel.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    state.read_with(&cx, |s, _| {
        let auth = s.pane_ref(id).unwrap().chat.auth.as_ref().unwrap();
        assert!(auth.custom_provider.is_none());
        assert!(auth.provider_operation.is_none());
    });
    assert!(cx.debug_bounds("add-compatible-provider").is_some());
}

#[test]
fn test_api_key_input_typing() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);

    cx.update_window_entity(&state, |s, window, cx| {
        s.open_agent_auth(id, AgentDialogPage::Auth, window, cx);
        s.select_provider_for_auth(id, "commandcode".into(), window, cx);
    });
    cx.draw_frame().unwrap();

    let focused = state.read_with(&cx, |s, _| {
        let auth = s.pane_ref(id).unwrap().chat.auth.as_ref().unwrap();
        auth.selected.clone()
    });
    assert_eq!(focused.as_deref(), Some("commandcode"));
    cx.run_until_parked();
    cx.draw_frame().unwrap();

    cx.simulate_input("my-secret-key-123");
    cx.draw_frame().unwrap();

    let (auth_val, bottom_val) = state.read_with(&cx, |s, cx| {
        let pane = s.pane_ref(id).unwrap();
        let auth = pane.chat.auth.as_ref().unwrap();
        let auth_v = auth.input.read(cx).value().to_string();
        let bottom_v = pane
            .input_state
            .as_ref()
            .unwrap()
            .read(cx)
            .value()
            .to_string();
        (auth_v, bottom_v)
    });
    assert_eq!(auth_val, "my-secret-key-123");
    assert_eq!(bottom_val, "");

    // Saving and disconnecting use the real credential store. Exercise those
    // operations in isolated processes, never against the user's account.
}

#[test]
fn model_favorites_filter_the_list_without_hiding_models() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    let model = agent_auth::AgentModel {
        provider: "openai".into(),
        id: "example-model".into(),
        backend: agent_auth::ModelBackend::Embedded,
    };
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_chat_mode(id, true, window, cx);
        s.open_agent_auth(id, AgentDialogPage::Models, window, cx);
        s.pane_mut(id).unwrap().chat.selected_model = Some(model.clone());
        s.agent_catalog.data.providers = vec![agent_auth::Provider {
            name: None,
            custom: false,
            id: "openai".into(),
            ready: true,
            source: None,
        }];
        s.agent_catalog.data.models = vec![model.clone()];
    });
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-model-0").is_some());
    let favorites = cx.debug_bounds("model-picker-favorites").unwrap();
    cx.simulate_click(favorites.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-model-0").is_none());

    cx.simulate_click(favorites.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    let star = cx.debug_bounds("favorite-model-0").unwrap();
    cx.simulate_click(star.center(), Modifiers::default());
    cx.simulate_click(favorites.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-model-0").is_some());
    state.read_with(&cx, |s, _| {
        assert_eq!(s.settings.favorite_agent_models, vec![model.clone()]);
        assert_eq!(
            s.pane_ref(id).unwrap().chat.selected_model.as_ref(),
            Some(&model)
        );
        let saved = serde_json::to_string(&s.saved_settings()).unwrap();
        let restored: settings::AppSettings = serde_json::from_str(&saved).unwrap();
        assert_eq!(restored.favorite_agent_models, vec![model.clone()]);
        let legacy = saved.replace("favorite_agent_models", "enabled_agent_models");
        let migrated: settings::AppSettings = serde_json::from_str(&legacy).unwrap();
        assert_eq!(migrated.favorite_agent_models, vec![model.clone()]);
    });

    let star = cx.debug_bounds("favorite-model-0").unwrap();
    cx.simulate_click(star.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-model-0").is_none());
    cx.simulate_click(favorites.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("agent-model-0").is_some());
}

#[test]
fn worked_header_toggles_progress_and_keeps_the_answer_visible() {
    fn capture(cx: &mut gpui::VisualTestContext, name: &str) {
        if let Ok(folder) = std::env::var("SHOB_CHAT_CAPTURE_DIR") {
            for _ in 0..8 { cx.run_until_parked(); cx.draw_commands(); }
            std::fs::create_dir_all(&folder).unwrap();
            let path = PathBuf::from(folder).join(name);
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                window.render_to_image().unwrap().save(path).unwrap();
                window.simulate_next_frame(cx);
            });
        }
    }
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let pane_id = state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |state, window, cx| {
        state.settings.agent_layout = true;
        state.sidebar_visible = false;
        state.theme_preference = ThemePreference::Dark;
        state.apply_theme_preference(Some(window), cx);
        state.set_chat_mode(pane_id, true, window, cx);
        let chat = &mut state.pane_mut(pane_id).unwrap().chat;
        let mut user = ChatMessage::new(ChatRole::User, "prompt".into());
        user.source_timestamp = Some(1_000);
        chat.messages.push(Arc::new(user));
        push_chat_event(&mut chat.messages, agent_bridge::AgentEvent::Text("I will inspect the files.".into()));
        push_chat_event(&mut chat.messages, agent_bridge::AgentEvent::ToolStart { id: Some("read".into()), name: "read".into(), input: "{}".into() });
        push_chat_event(&mut chat.messages, agent_bridge::AgentEvent::ToolEnd { id: Some("read".into()), name: "read".into(), output: "contents".into(), failed: false });
        push_chat_event(&mut chat.messages, agent_bridge::AgentEvent::Text("The final answer.".into()));
        finish_chat_streaming(&mut chat.messages);
        finish_worked_turn(&mut chat.messages, Some(10_000));
        chat.mark_active_dirty();
        cx.notify();
    });
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("chat-message-1").is_none());
    assert!(cx.debug_bounds("chat-message-3").is_some());
    capture(&mut cx, "worked-collapsed.png");
    let header = cx.debug_bounds("worked-turn-1").expect("completed turn header");
    cx.simulate_click(header.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("chat-message-1").is_some());
    assert!(cx.debug_bounds("tool-group-2").is_some());
    assert!(cx.debug_bounds("chat-message-3").is_some());
    capture(&mut cx, "worked-expanded.png");
    let header = cx.debug_bounds("worked-turn-1").unwrap();
    cx.simulate_click(header.center(), Modifiers::default());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("chat-message-1").is_none());
    assert!(cx.debug_bounds("tool-group-2").is_none());
    assert!(cx.debug_bounds("chat-message-3").is_some());

    cx.update_window_entity(&state, |state, _, cx| {
        let chat = &mut state.pane_mut(pane_id).unwrap().chat;
        for index in 0..160 {
            chat.messages.push(Arc::new(ChatMessage::new(ChatRole::Assistant, format!("Later message {index}"))));
        }
        chat.list_state.scroll_to(gpui::ListOffset { item_ix: 1, offset_in_item: px(0.0) });
        cx.notify();
    });
    cx.draw_frame().unwrap();
    for _ in 0..2 {
        let header = cx.debug_bounds("worked-turn-1").unwrap();
        cx.simulate_click(header.center(), Modifiers::default());
        cx.draw_frame().unwrap();
        state.read_with(&cx, |state, _| {
            let list = &state.pane_ref(pane_id).unwrap().chat.list_state;
            assert_eq!(list.logical_scroll_top().item_ix, 1);
            assert_eq!(list.is_scrolled_to_end(), Some(false), "folding history must not discard the following rows' height map");
        });
    }
}
