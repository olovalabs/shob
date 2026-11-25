use super::*;
use gpui::{Modifiers, VisualContext, point};
use resize_tests::{DrawFrame, TestAppContext, open_app};

fn wait_for_terminal_submission(
    cx: &mut gpui::VisualTestContext,
    state: &Entity<AppState>,
    pane_id: usize,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        cx.run_until_parked();
        if !state.read_with(cx, |state, _| {
            state.pane_ref(pane_id).unwrap().input_submitting
        }) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "PTY submission did not settle"
        );
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(50));
        std::thread::yield_now();
    }
}

#[test]
fn failed_physical_pty_write_keeps_draft_and_never_sends_enter() {
    struct FailingWriter(Arc<std::sync::atomic::AtomicUsize>);
    impl std::io::Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "closed test PTY",
            ))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut cx = TestAppContext::new();
    let (mut cx, app) = open_app(&mut cx);
    cx.draw_frame().unwrap();
    let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let pane_id = app.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&app, |state, window, cx| {
        let pane = state.pane_mut(pane_id).unwrap();
        pane.terminal = cx.new(|cx| {
            TerminalView::new(
                FailingWriter(writes.clone()),
                std::io::empty(),
                TerminalConfig::default(),
                cx,
            )
        });
        pane.input_state.as_ref().unwrap().update(cx, |input, cx| {
            input.set_value("do not lose this", window, cx)
        });
        state.execute_pane_input(pane_id, window, cx);
    });
    wait_for_terminal_submission(&mut cx, &app, pane_id);
    app.read_with(&cx, |state, cx| {
        let pane = state.pane_ref(pane_id).unwrap();
        assert_eq!(
            pane.input_state.as_ref().unwrap().read(cx).value().as_str(),
            "do not lose this"
        );
        assert!(pane.command_history.is_empty());
    });
    assert_eq!(
        writes.load(Ordering::Relaxed),
        1,
        "failed input must not be replayed or followed by Enter"
    );
    cx.draw_frame().unwrap();
}

#[test]
fn entering_agent_mode_without_a_model_does_not_start_a_backend() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_chat_mode(pane_id, true, window, cx);
        let chat = &s.pane_ref(pane_id).unwrap().chat;
        assert!(chat.active);
        assert!(chat.selected_model.is_none());
        assert!(chat.bridge.is_none());
    });
}

#[test]
fn streamed_thinking_and_answers_keep_their_boundaries() {
    use agent_bridge::AgentEvent;
    let mut messages = Vec::new();
    push_chat_event(&mut messages, AgentEvent::Thinking("Checking ".into()));
    push_chat_event(&mut messages, AgentEvent::Thinking("files".into()));
    push_chat_event(&mut messages, AgentEvent::Text("Found ".into()));
    push_chat_event(&mut messages, AgentEvent::Text("the issue".into()));
    push_chat_event(&mut messages, AgentEvent::Tool("read finished".into()));
    push_chat_event(&mut messages, AgentEvent::Text("Fixed it".into()));
    assert_eq!(messages.len(), 4);
    assert!(messages[0].role == ChatRole::Thinking && messages[0].text == "Checking files");
    assert!(messages[1].role == ChatRole::Assistant && messages[1].text == "Found the issue");
    assert!(messages[2].role == ChatRole::Tool && messages[2].text == "read finished");
    assert!(messages[3].role == ChatRole::Assistant && messages[3].text == "Fixed it");
}

#[test]
fn streaming_split_keeps_fenced_blocks_whole() {
    let text = "intro\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\ntail";
    let (settled, tail) = split_streaming_markdown(text);
    assert_eq!(
        settled.trim_end(),
        "intro\n\n```rust\nfn a() {}\n\nfn b() {}\n```"
    );
    assert_eq!(tail, "tail");

    let (settled, tail) = split_streaming_markdown("first\n\nsecond");
    assert_eq!(settled.trim_end(), "first");
    assert_eq!(tail, "second");

    // A list still being written stays whole so numbering/nesting keep context.
    let (settled, tail) = split_streaming_markdown("Intro\n\n- one\n- two\n- three");
    assert_eq!(settled.trim_end(), "Intro");
    assert_eq!(tail, "- one\n- two\n- three");

    let (settled, tail) = split_streaming_markdown("Intro\n\n```\n    ```\n\ninside");
    assert_eq!(settled.trim_end(), "Intro");
    assert!(tail.contains("inside"));

    for list in [
        "7. first\n8. second\n   - nested\n\n     continuation\n9. third",
        "- first\n\n  ```rust\n  let x = 1;\n  ```\n\n- second\n\nafter",
        "- first\n  - nested\n\n    paragraph\nlazy continuation\n\n- last",
    ] {
        let text = format!("Intro\n\n{list}");
        let (settled, tail) = split_streaming_markdown(&text);
        assert_eq!(settled.trim_end(), "Intro");
        assert_eq!(
            tail, list,
            "list numbering/nesting must share one parser input"
        );
    }
}

#[test]
fn oversized_streaming_markdown_bounds_live_work_and_preserves_final_code() {
    use agent_bridge::AgentEvent;
    for delta in ["let value = \"世界🦀\";\n", "世界🦀"] {
        let mut messages = Vec::new();
        push_chat_event(&mut messages, AgentEvent::Text("Intro\n\n```rust\n".into()));
        for _ in 0..2_048 {
            push_chat_event(&mut messages, AgentEvent::Text(delta.into()));
            let message = messages.last().unwrap();
            let view = message.stream.as_ref().unwrap();
            assert!(view.tail.len() <= STREAM_LIVE_BYTES);
            assert!(message.text.ends_with(view.tail.as_ref()));
            assert_eq!(view.settled.chunk(0).as_deref(), Some("Intro\n\n"));
        }
        assert!(messages[0].stream.as_ref().unwrap().plain_tail);
        push_chat_event(&mut messages, AgentEvent::Text("\n```\n\nAfter".into()));
        assert!(
            messages[0].stream.as_ref().unwrap().plain_tail,
            "closing an oversized fence must not submit the entire block to the live parser"
        );
        let complete = messages[0].text.clone();
        finish_chat_streaming(&mut messages);
        assert!(!messages[0].streaming);
        assert!(messages[0].stream.is_none());
        assert_eq!(messages[0].text, complete);
        let code = pulldown_cmark::Parser::new(&complete)
            .filter_map(|event| match event {
                pulldown_cmark::Event::Text(text) => Some(text.into_string()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(code, format!("Intro{}After", delta.repeat(2_048) + "\n"));
    }

    let mut list = Vec::new();
    push_chat_event(&mut list, AgentEvent::Text("7. first\n".into()));
    for _ in 0..512 {
        push_chat_event(
            &mut list,
            AgentEvent::Text("8. next\n   - nested 世界🦀\n".into()),
        );
        assert!(list[0].stream.as_ref().unwrap().tail.len() <= STREAM_LIVE_BYTES);
    }
    assert!(list[0].stream.as_ref().unwrap().plain_tail);
    finish_chat_streaming(&mut list);
    let lists = pulldown_cmark::Parser::new(&list[0].text)
        .filter_map(|event| match event {
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::List(start)) => Some(start),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(lists.first(), Some(&Some(7)));
    assert_eq!(lists.iter().filter(|start| start.is_some()).count(), 1);
    assert_eq!(lists.iter().filter(|start| start.is_none()).count(), 512);

    // A single large bridge batch must also avoid an initial unbounded scan.
    let mut message = ChatMessage::new(ChatRole::Assistant, "🦀".repeat(STREAM_LIVE_BYTES));
    refresh_stream_view(&mut message, true);
    let view = message.stream.as_ref().unwrap();
    assert!(view.plain_tail);
    assert!(view.settled.is_empty());
    assert!(view.tail.len() <= STREAM_LIVE_BYTES);
}

#[test]
fn oversized_live_preview_copies_full_message_and_finalizes_to_markdown() {
    use agent_bridge::AgentEvent;
    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    let pane_id = state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);
    let text = format!("```rust\n{}\n```", "let x = 1; ".repeat(1_000));
    cx.update_window_entity(&state, |state, window, cx| {
        state.set_chat_mode(pane_id, true, window, cx);
        let chat = &mut state.pane_mut(pane_id).unwrap().chat;
        let event = AgentEvent::Text(text.clone());
        chat.observe_status(&event);
        push_chat_event(&mut chat.messages, event);
        cx.notify();
    });
    cx.draw_frame().unwrap();
    cx.run_until_parked();
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("stream-live-preview").is_some());
    let copy = cx
        .debug_bounds("stream-copy-full-message")
        .expect("complete live copy action");
    let viewport = cx.debug_bounds("agent-chat").unwrap();
    assert!(
        copy.size.height > px(0.0)
            && copy.center().y >= viewport.top()
            && copy.center().y < viewport.bottom(),
        "live copy action must be reachable: copy={copy:?}, viewport={viewport:?}"
    );
    cx.simulate_click(copy.center(), Modifiers::default());
    assert_eq!(
        cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
        Some(text.clone())
    );
    cx.update_window_entity(&state, |state, _, cx| {
        let chat = &mut state.pane_mut(pane_id).unwrap().chat;
        chat.observe_status(&AgentEvent::Done);
        finish_chat_streaming(&mut chat.messages);
        cx.notify();
    });
    cx.draw_frame().unwrap();
    cx.run_until_parked();
    cx.draw_frame().unwrap();
    cx.update_window_entity(&state, |state, _, cx| {
        let chat = &mut state.pane_mut(pane_id).unwrap().chat;
        chat.list_state.scroll_to(gpui::ListOffset {
            item_ix: 0,
            offset_in_item: px(0.0),
        });
        cx.notify();
    });
    cx.draw_frame().unwrap();
    let code_copy = cx
        .debug_bounds("chat-code-copy")
        .expect("final fenced code has a copy action");
    let viewport = cx.debug_bounds("agent-chat").unwrap();
    assert!(
        code_copy.center().y >= viewport.top()
            && code_copy.center().y < viewport.bottom()
            && code_copy.center().x >= viewport.left()
            && code_copy.center().x < viewport.right(),
        "code copy action must be reachable: copy={code_copy:?}, viewport={viewport:?}"
    );
    cx.simulate_click(code_copy.center(), Modifiers::default());
    assert_eq!(
        cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
        Some("let x = 1; ".repeat(1_000))
    );
    assert!(cx.debug_bounds("stream-live-preview").is_none());
    assert!(cx.debug_bounds("stream-copy-full-message").is_none());
    state.read_with(&cx, |state, _| {
        let message = &state.pane_ref(pane_id).unwrap().chat.visible_messages()[0];
        assert_eq!(message.text, text);
        assert!(message.stream.is_none());
    });
}

#[test]
fn completed_long_message_virtualizes_blocks_in_the_outer_chat_scroll() {
    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    let pane_id = state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);
    let text: String = (0..2_000)
        .map(|index| format!("Paragraph {index} **bold**.\n\n"))
        .collect();
    cx.update_window_entity(&state, |state, window, cx| {
        state.set_chat_mode(pane_id, true, window, cx);
        let chat = &mut state.pane_mut(pane_id).unwrap().chat;
        chat.messages.push(Arc::new(ChatMessage::new(
            ChatRole::Assistant,
            text.clone(),
        )));
        chat.notify_content(cx);
        cx.notify();
    });
    cx.draw_frame().unwrap();
    cx.run_until_parked();
    cx.draw_frame().unwrap();
    cx.update_window_entity(&state, |state, _, cx| {
        let chat = &mut state.pane_mut(pane_id).unwrap().chat;
        chat.list_state.scroll_to(gpui::ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        });
        chat.notify_content(cx);
    });
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("markdown-flow-block-0").is_some());
    assert!(
        cx.debug_bounds("markdown-flow-block-1999").is_none(),
        "offscreen blocks must not build elements in a long message"
    );
    state.read_with(&cx, |state, _| {
        let chat = &state.pane_ref(pane_id).unwrap().chat;
        assert_eq!(
            chat.list_state.item_count(),
            1,
            "one outer scroll, not a nested list"
        );
        assert_eq!(chat.messages[0].text, text);
    });

    let old_estimate = px(16_000.0);
    cx.update_window_entity(&state, |state, _, cx| {
        let chat = &state.pane_ref(pane_id).unwrap().chat;
        chat.list_state.scroll_to(gpui::ListOffset { item_ix: 0, offset_in_item: old_estimate });
        chat.notify_content(cx);
    });
    cx.draw_frame().unwrap();
    let viewport = cx.debug_bounds("agent-chat").unwrap();
    let (key, y) = [
        "markdown-flow-block-496",
        "markdown-flow-block-504",
        "markdown-flow-block-512",
        "markdown-flow-block-520",
        "markdown-flow-block-528",
    ]
        .into_iter()
        .find_map(|key| {
            let bounds = cx.debug_bounds(key)?;
            (bounds.bottom() > viewport.top() && bounds.top() < viewport.bottom())
                .then_some((key, bounds.top()))
        })
        .expect("a block intersects the viewport after a jump into unmeasured Markdown");
    state.read_with(&cx, |state, _| {
        let list = &state.pane_ref(pane_id).unwrap().chat.list_state;
        assert_ne!(list.logical_scroll_top().offset_in_item, old_estimate, "height deltas above the anchor must reach the outer list");
    });
    cx.draw_frame().unwrap();
    assert!((cx.debug_bounds(key).unwrap().top() - y).abs() <= px(1.0));

    let inset = y - viewport.top();
    for width in [640., 800., 650.] {
        cx.simulate_resize(size(px(width), px(720.)));
        cx.draw_frame().unwrap();
        let viewport = cx.debug_bounds("agent-chat").unwrap();
        let block = cx.debug_bounds(key).expect("same Markdown block after resize");
        assert!((block.top() - viewport.top() - inset).abs() <= px(1.0));
    }
}

#[test]
fn long_markdown_stays_above_the_following_prompt_during_reflow() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    let text = format!(
        "# First section\n\n```rust\n{}```\n\n## Second section\n\n{}\n\nFinal paragraph.",
        "let value = 42;\n".repeat(16),
        "Words that wrap to another line at narrow widths. ".repeat(10),
    );
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_chat_mode(pane_id, true, window, cx);
        let chat = &mut s.pane_mut(pane_id).unwrap().chat;
        chat.messages.extend([
            Arc::new(ChatMessage::new(ChatRole::Assistant, text)),
            Arc::new(ChatMessage::new(ChatRole::User, "do we have to?".into())),
        ]);
        chat.notify_content(cx);
        cx.notify();
    });

    let check_stacking = |cx: &mut gpui::VisualTestContext| {
        let prompt = cx
            .debug_bounds("chat-user-bubble")
            .expect("following prompt");
        for key in [
            "markdown-flow-block-0",
            "markdown-flow-block-1",
            "markdown-flow-block-2",
            "markdown-flow-block-3",
            "markdown-flow-block-4",
        ] {
            if let Some(block) = cx.debug_bounds(key) {
                assert!(
                    block.top() < prompt.top(),
                    "{key} starts below the prompt: {block:?} vs {prompt:?}"
                );
            }
        }
        prompt
    };
    cx.draw_frame().unwrap();
    cx.run_until_parked();
    cx.draw_frame().unwrap();
    check_stacking(&mut cx);
    cx.draw_frame().unwrap();
    let prompt = check_stacking(&mut cx);
    let last = cx.debug_bounds("markdown-flow-block-4").expect("final paragraph");
    assert!(last.bottom() < prompt.top());

    for width in [640., 860., 660.] {
        cx.simulate_resize(size(px(width), px(920.)));
        cx.draw_frame().unwrap();
        check_stacking(&mut cx);
        cx.draw_frame().unwrap();
        let prompt = check_stacking(&mut cx);
        let last = cx
            .debug_bounds("markdown-flow-block-4")
            .expect("final paragraph after resize");
        assert!(last.bottom() < prompt.top());
        cx.draw_frame().unwrap();
        assert_eq!(check_stacking(&mut cx), prompt);
    }

    cx.simulate_resize(size(px(660.), px(560.)));
    cx.draw_frame().unwrap();
    let before_scroll = check_stacking(&mut cx);
    let position = cx.debug_bounds("agent-chat").unwrap().center();
    for delta in [28., -28.] {
        cx.simulate_event(gpui::ScrollEvent {
            position,
            delta: gpui::ScrollDelta::Pixels(point(px(0.), px(delta))),
            ..Default::default()
        });
        cx.draw_frame().unwrap();
        let prompt = check_stacking(&mut cx);
        let last = cx.debug_bounds("markdown-flow-block-4").expect("final paragraph while scrolling");
        assert!(last.bottom() < prompt.top());
        if delta > 0. {
            assert!(prompt.top() > before_scroll.top());
        } else {
            assert_eq!(prompt, before_scroll);
        }
    }
}

#[test]
fn wide_markdown_table_scrolls_without_moving_the_transcript() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let pane = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    let headings = (0..12).map(|i| format!("Column {i}")).collect::<Vec<_>>().join(" | ");
    let row = (0..12).map(|i| format!("`very_long_unbroken_identifier_in_cell_{i:02}`")).collect::<Vec<_>>().join(" | ");
    let markdown = format!("| {headings} |\n| {} |\n| {row} |\n", ["---"; 12].join(" | "));
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_chat_mode(pane, true, window, cx);
        s.pane_mut(pane).unwrap().chat.messages.push(Arc::new(ChatMessage::new(ChatRole::Assistant, markdown)));
        cx.notify();
    });
    cx.draw_frame().unwrap();
    cx.run_until_parked();
    cx.draw_frame().unwrap();

    let viewport = cx.debug_bounds("markdown-table-scroll").expect("table scroll viewport");
    let first = cx.debug_bounds("markdown-table-column-0").unwrap();
    let second = cx.debug_bounds("markdown-table-column-1").unwrap();
    let right = cx.debug_bounds("markdown-table-column-11").unwrap();
    assert!(first.size.width >= px(96.0));
    assert!(first.right() <= second.left());
    assert!(first.size.height >= px(48.0), "the identifier wraps within its cell");
    assert!(right.right() > viewport.right());
    cx.simulate_event(gpui::ScrollEvent {
        position: viewport.center(),
        delta: gpui::ScrollDelta::Pixels(point(px(-8_000.0), px(0.0))),
        ..Default::default()
    });
    cx.draw_frame().unwrap();
    let revealed = cx.debug_bounds("markdown-table-column-11").unwrap();
    let header = cx.debug_bounds("markdown-table-header-11").unwrap();
    assert!(revealed.right() <= viewport.right());
    assert!(revealed.left() >= viewport.left());
    assert_eq!(revealed.left(), header.left(), "header and body must scroll together");
    assert_eq!(revealed.top(), right.top());

    cx.simulate_event(gpui::ScrollEvent {
        position: viewport.center(),
        delta: gpui::ScrollDelta::Pixels(point(px(0.0), px(8_000.0))),
        modifiers: gpui::Modifiers { shift: true, ..Default::default() },
        ..Default::default()
    });
    cx.draw_frame().unwrap();
    assert_eq!(cx.debug_bounds("markdown-table-column-0"), Some(first));
}

#[test]
fn text_events_stream_until_the_next_boundary() {
    use agent_bridge::AgentEvent;
    let mut messages = Vec::new();
    push_chat_event(&mut messages, AgentEvent::Text("Work".into()));
    assert!(messages[0].streaming);
    push_chat_event(&mut messages, AgentEvent::Tool("read".into()));
    assert!(!messages[0].streaming);
    push_chat_event(&mut messages, AgentEvent::Text("Done".into()));
    assert!(messages[2].streaming);
    finish_chat_streaming(&mut messages);
    assert!(!messages[2].streaming);
}

#[test]
fn starting_a_tool_finalizes_the_streaming_assistant_message() {
    use agent_bridge::AgentEvent;
    let mut messages = Vec::new();
    push_chat_event(&mut messages, AgentEvent::Text("answer".into()));
    assert!(messages[0].streaming);
    push_chat_event(
        &mut messages,
        AgentEvent::ToolStart {
            id: Some("t1".into()),
            name: "read".into(),
            input: "{}".into(),
        },
    );
    assert!(
        !messages[0].streaming,
        "the prior assistant text stops streaming when a tool starts"
    );
    assert!(messages[0].stream.is_none());
}

#[test]
fn streamed_markdown_preserves_content_and_block_boundaries_across_chunkings() {
    let examples = [
        "intro\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\ntail",
        "Intro\n\n- one\n- two\n\n  continuation\n\nafter\n\nnext",
        "a\n\n~~~\nx\n\n~~~\n\nnext",
        "# title\n\n|a|b|\n|-|-|\n|x|y|\n\nnext",
        "hello\r\n\r\n世界\n\nemoji 🦀",
    ];
    for text in examples {
        let chars: Vec<_> = text.chars().collect();
        for size in 1..=chars.len() {
            let mut message = ChatMessage::new(ChatRole::Assistant, String::new());
            refresh_stream_view(&mut message, true);
            for chunk in chars.chunks(size) {
                let delta: String = chunk.iter().collect();
                message.text.push_str(&delta);
                refresh_stream_view(&mut message, delta.contains('\n'));
                let view = message.stream.as_ref().unwrap();
                let settled: String = (0..)
                    .map_while(|index| view.settled.chunk(index))
                    .map(|chunk| chunk.to_string())
                    .collect();
                assert_eq!(format!("{settled}{}", view.tail), message.text);
                if delta.contains('\n') {
                    let (_, tail) = split_streaming_markdown(&message.text);
                    assert_eq!(
                        view.tail.as_ref(),
                        tail,
                        "size={size}, text={:?}",
                        message.text
                    );
                }
            }
            assert_eq!(message.text, text);
        }
    }
}

#[test]
fn streaming_settled_chunks_are_append_only_across_thousands_of_paragraphs() {
    let mut message = ChatMessage::new(ChatRole::Assistant, String::new());
    message.streaming = true;
    let mut first = None;
    for index in 0..4_000 {
        message
            .text
            .push_str(&format!("paragraph {index} **bold** 世界🦀\n\n"));
        refresh_stream_view(&mut message, true);
        let view = message.stream.as_ref().unwrap();
        let chunk = view.settled.chunk(0).unwrap();
        let address = chunk.as_ptr();
        assert_eq!(*first.get_or_insert(address), address);
        assert_eq!(view.settled.len() + view.tail.len(), message.text.len());
        assert!(view.tail.len() <= STREAM_LIVE_BYTES);
    }
    let view = message.stream.as_ref().unwrap();
    let settled: String = (0..)
        .map_while(|index| view.settled.chunk(index))
        .map(|chunk| chunk.to_string())
        .collect();
    assert_eq!(format!("{settled}{}", view.tail), message.text);
    let expected = message.text.clone();
    let mut messages = vec![Arc::new(message)];
    finish_chat_streaming(&mut messages);
    assert_eq!(messages[0].text, expected);
    assert!(!messages[0].streaming);
}

#[test]
fn deterministic_replay_matches_the_live_transcript() {
    use agent_bridge::AgentEvent;
    let batch = vec![
        AgentEvent::Thinking("plan ".into()),
        AgentEvent::Thinking("carefully".into()),
        AgentEvent::Text("hello ".into()),
        AgentEvent::Text("world".into()),
        AgentEvent::ToolStart {
            id: Some("t1".into()),
            name: "bash".into(),
            input: r#"{"command":"printf hi"}"#.into(),
        },
        AgentEvent::ToolUpdate {
            id: Some("t1".into()),
            name: "bash".into(),
            output: "h".into(),
        },
        AgentEvent::ToolUpdate {
            id: Some("t1".into()),
            name: "bash".into(),
            output: "hi".into(),
        },
        AgentEvent::ToolEnd {
            id: Some("t1".into()),
            name: "bash".into(),
            output: "hi\n".into(),
            failed: false,
        },
        AgentEvent::Text("done".into()),
        AgentEvent::Done,
        AgentEvent::Closed,
    ];

    let mut live = Vec::new();
    for event in batch.iter().cloned() {
        match event {
            AgentEvent::Done | AgentEvent::Closed => {
                finish_chat_tools(&mut live);
                finish_chat_streaming(&mut live);
            }
            other => push_chat_event(&mut live, other),
        }
    }

    let mut replay = Vec::new();
    for event in agent_bridge::coalesce_events(batch) {
        match event {
            AgentEvent::Done | AgentEvent::Closed => {
                finish_chat_tools(&mut replay);
                finish_chat_streaming(&mut replay);
            }
            other => push_chat_event(&mut replay, other),
        }
    }

    assert_eq!(live.len(), replay.len());
    for (live_message, replay_message) in live.iter().zip(&replay) {
        assert_eq!(live_message.role, replay_message.role);
        assert_eq!(live_message.text, replay_message.text);
        assert_eq!(live_message.streaming, replay_message.streaming);
        match (&live_message.tool, &replay_message.tool) {
            (Some(live_tool), Some(replay_tool)) => {
                assert_eq!(live_tool.name, replay_tool.name);
                assert_eq!(live_tool.output, replay_tool.output);
                assert_eq!(live_tool.state, replay_tool.state);
                assert_eq!(live_tool.display.view.title, replay_tool.display.view.title);
            }
            (None, None) => {}
            _ => panic!("tool shape differs between live and replayed transcript"),
        }
    }

    let tool = replay
        .iter()
        .find_map(|message| message.tool.as_ref())
        .expect("the tool card replays");
    assert_eq!(tool.output.as_ref(), "hi\n");
    assert_eq!(tool.state, ToolState::Completed);
    assert!(
        replay
            .iter()
            .any(|message| message.role == ChatRole::Assistant && message.text == "done"),
        "text after a tool call starts a new message"
    );
}

#[test]
fn web_sources_extracts_unique_sites_from_results() {
    let output = "[first](https://www.example.com/a) https://example.com/b \"https://news.example.org/story\"";
    assert_eq!(
        web_sources("web_search", output),
        ["example.com", "news.example.org"]
    );
    assert_eq!(
        web_sources("web_fetch", "https://example.com/page"),
        ["example.com"]
    );
    assert!(web_sources("shell", output).is_empty());
}

#[test]
fn oversized_tool_output_is_previewed_without_losing_the_full_text() {
    let text = "line\n".repeat(1_000);
    let (preview_len, truncated) = output_preview_len(&text);
    assert!(truncated);
    assert!(preview_len <= TOOL_OUTPUT_PREVIEW_BYTES);
    assert!(preview_len < text.len());

    let (full, preview, truncated) = tool_output_fields(text.clone());
    assert!(truncated);
    assert_eq!(full.len(), text.len());
    assert_eq!(preview.len(), preview_len);

    let (_, preview, truncated) = tool_output_fields("ok".to_string());
    assert!(!truncated);
    assert_eq!(preview.as_ref(), "ok");
}

#[test]
fn tool_inputs_keep_full_text_and_bound_the_initial_preview() {
    let input = format!(r#"{{"payload":"{}"}}"#, "x".repeat(20_000));
    let display = ToolDisplay::new("custom_tool", &input);
    assert_eq!(display.input.as_ref(), input);
    assert!(display.input_truncated);
    assert!(display.input_preview.len() <= TOOL_OUTPUT_PREVIEW_BYTES);

    let shell = ToolDisplay::new("bash", r#"{"command":"false"}"#);
    assert_eq!(shell.shell_command.as_deref(), Some("false"));
    let long_command = "printf λ\n".repeat(2_000);
    let shell = ToolDisplay::new(
        "bash",
        &serde_json::json!({"command": long_command}).to_string(),
    );
    assert_eq!(shell.shell_command.as_deref(), Some(long_command.as_str()));
    assert!(shell.shell_truncated);
    assert!(shell.shell_preview.as_ref().unwrap().len() <= TOOL_OUTPUT_PREVIEW_BYTES);
    assert!(ToolDisplay::new("shell", r#"{"unsupported":"value"}"#).show_raw_input);

    let described = ToolDisplay::new(
        "powershell",
        r#"{"command":"Get-ChildItem","description":"Inspect project files"}"#,
    );
    assert_eq!(described.shell_command.as_deref(), Some("Get-ChildItem"));
    assert_eq!(described.view.subtitle, "Inspect project files");
    let metadata = ToolDisplay::new(
        "shell",
        r#"{"metadata":{"command":"printf ok","title":"Check output"}}"#,
    );
    assert_eq!(metadata.shell_command.as_deref(), Some("printf ok"));
    assert_eq!(metadata.view.subtitle, "Check output");
    assert_eq!(
        ToolDisplay::new("bash", r#"{"command":null,"cmd":"echo ok"}"#)
            .shell_command
            .as_deref(),
        Some("echo ok")
    );

    let write = ToolDisplay::new(
        "write_file",
        r##"{"file_path":"a.rs","content":"# literal\n\tx < y"}"##,
    );
    assert_eq!(write.content_body.as_deref(), Some("# literal\n\tx < y"));
    assert!(!write.show_raw_input);
    let edit = ToolDisplay::new(
        "edit",
        r#"{"path":"a.rs","edits":[{"oldText":"old\nline","newText":"new\nline"},{"oldText":"gone","newText":""}]}"#,
    );
    let diff = edit.content_body.as_deref().expect("embedded edit diff");
    assert!(diff.contains("-old\n-line\n+new\n+line"));
    assert!(diff.contains("-gone"));
    assert!(!edit.show_raw_input);
    assert!(ToolDisplay::new("edit", r#"{"path":"a.rs","unsupported":"value"}"#).show_raw_input);
    assert!(ToolDisplay::new("custom_tool", r#"{"payload":"raw"}"#).show_raw_input);
}

#[test]
fn concurrent_tool_snapshots_stay_with_their_original_calls() {
    use agent_bridge::AgentEvent;
    let mut messages = Vec::new();
    for id in ["a", "b"] {
        push_chat_event(
            &mut messages,
            AgentEvent::ToolStart {
                id: Some(id.into()),
                name: "read".into(),
                input: format!("input {id}"),
            },
        );
    }
    push_chat_event(&mut messages, AgentEvent::Thinking("Checking".into()));
    push_chat_event(
        &mut messages,
        AgentEvent::ToolUpdate {
            id: Some("b".into()),
            name: "read".into(),
            output: "partial".into(),
        },
    );
    push_chat_event(
        &mut messages,
        AgentEvent::ToolEnd {
            id: Some("a".into()),
            name: "read".into(),
            output: "result a".into(),
            failed: false,
        },
    );
    push_chat_event(
        &mut messages,
        AgentEvent::ToolEnd {
            id: Some("b".into()),
            name: "read".into(),
            output: "result b".into(),
            failed: true,
        },
    );
    assert_eq!(messages.len(), 3);
    let a = messages[0].tool.as_ref().unwrap();
    let b = messages[1].tool.as_ref().unwrap();
    assert!(a.output.as_ref() == "result a" && a.state == ToolState::Completed);
    assert!(b.output.as_ref() == "result b" && b.state == ToolState::Failed);
    assert!(messages[2].role == ChatRole::Thinking);
}

#[test]
fn completed_reasoning_collapses_when_the_answer_starts() {
    use agent_bridge::AgentEvent;
    let mut messages = Vec::new();
    push_chat_event(&mut messages, AgentEvent::Thinking("Looking ".into()));
    push_chat_event(&mut messages, AgentEvent::Thinking("closely".into()));
    assert_eq!(messages.len(), 1);
    assert!(!messages[0].expanded, "live reasoning starts folded");
    assert_eq!(messages[0].text, "Looking closely");
    push_chat_event(&mut messages, AgentEvent::Text("Done".into()));
    assert!(
        !messages[0].expanded,
        "completed reasoning folds when the answer begins"
    );
    assert!(!messages[0].streaming);
}

#[test]
fn subagent_rows_open_a_separate_view_and_return_to_parent() {
    use agent_bridge::AgentEvent;

    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |state, _, cx| {
        let pane = state.pane_mut(pane_id).unwrap();
        pane.chat.active = true;
        for (id, name, output) in [
            (
                "spawn",
                "task",
                "Spawned 2 background agents using scout.
- `FirstWorker` (job `FirstWorker`)
- `SecondWorker` (job `SecondWorker`)",
            ),
            (
                "wait",
                "wait",
                "## Completed (1)

### FirstWorker [task] — completed
<task-result id=\"FirstWorker\">
<output>
First report
</output>
</task-result>

## Still Running (1)
- `SecondWorker` [task] — SecondWorker",
            ),
        ] {
            push_chat_event(
                &mut pane.chat.messages,
                AgentEvent::ToolEnd {
                    id: Some(id.into()),
                    name: name.into(),
                    output: output.into(),
                    failed: false,
                },
            );
        }
        let rows = subagent_chat::rows(&pane.chat.messages);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].worker.status_label(), "Completed");
        assert_eq!(rows[0].result, Some("First report"));
        assert_eq!(rows[1].worker.status_label(), "Running");
        cx.notify();
    });
    cx.draw_frame().expect("draw parent chat");
    assert!(cx.debug_bounds("tool-row-0").is_none());
    assert!(cx.debug_bounds("subagent-list-0").is_some());
    let row = cx
        .debug_bounds("subagent-row-FirstWorker")
        .expect("first worker");
    assert!(cx.debug_bounds("subagent-row-SecondWorker").is_some());
    cx.simulate_click(row.origin + point(px(20.0), px(10.0)), Modifiers::default());
    cx.draw_frame().expect("draw worker chat");
    assert!(cx.debug_bounds("subagent-transcript").is_some());
    assert!(cx.debug_bounds("subagent-row-SecondWorker").is_none());
    let back = cx.debug_bounds("subagent-back").expect("back button");
    cx.simulate_click(
        back.origin + point(px(20.0), px(10.0)),
        Modifiers::default(),
    );
    cx.draw_frame().expect("draw parent chat again");
    assert!(cx.debug_bounds("subagent-transcript").is_none());
    assert!(cx.debug_bounds("subagent-row-FirstWorker").is_some());
}

#[test]
fn subagent_view_matches_main_chat_column_width_and_row_grouping() {
    use agent_bridge::AgentEvent;

    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.simulate_resize(gpui::size(px(1400.0), px(800.0)));
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |state, _, cx| {
        state.settings.agent_layout = true;
        let pane = state.pane_mut(pane_id).unwrap();
        pane.chat.active = true;
        push_chat_event(
            &mut pane.chat.messages,
            AgentEvent::ToolStart {
                id: Some("sub-1".into()),
                name: embedded::subagent_tool::NAME.into(),
                input: String::new(),
            },
        );
        push_chat_event(
            &mut pane.chat.messages,
            AgentEvent::Subagents {
                tool_id: "sub-1".into(),
                workers: vec![subagent_chat::Worker {
                    id: "Explorer".into(),
                    title: "Explorer".into(),
                    status: "running".into(),
                    transcript: None,
                    result: None,
                }],
            },
        );
        cx.notify();
    });
    cx.draw_frame().expect("draw parent chat");
    let row = cx
        .debug_bounds("subagent-row-Explorer")
        .expect("explorer worker");
    cx.simulate_click(row.origin + point(px(20.0), px(10.0)), Modifiers::default());
    cx.update_window_entity(&state, |state, _, cx| {
        let pane = state.pane_mut(pane_id).unwrap();
        push_chat_event(
            &mut pane.chat.messages,
            AgentEvent::ToolUpdate {
                id: Some("sub-1".into()),
                name: embedded::subagent_tool::NAME.into(),
                output: "Native worker preview".into(),
            },
        );
        let view = pane.chat.worker_view.as_mut().expect("worker view open");
        let mut messages = vec![Arc::new(ChatMessage::new(
            ChatRole::User,
            "Inspect terminal hot path".into(),
        ))];
        push_chat_event(
            &mut messages,
            AgentEvent::ToolStart {
                id: Some("t1".into()),
                name: "bash".into(),
                input: "{\"command\":\"cat Cargo.toml\"}".into(),
            },
        );
        push_chat_event(
            &mut messages,
            AgentEvent::ToolEnd {
                id: Some("t1".into()),
                name: "bash".into(),
                output: "ok".into(),
                failed: false,
            },
        );
        push_chat_event(
            &mut messages,
            AgentEvent::Text("Docs read. Confirming caps.".into()),
        );
        push_chat_event(
            &mut messages,
            AgentEvent::ToolStart {
                id: Some("t2".into()),
                name: "read".into(),
                input: "{\"path\":\"src/main.rs\"}".into(),
            },
        );
        AppState::sync_worker_messages(view, messages);
        pane.chat.notify_worker_view(cx);
        cx.notify();
    });
    cx.draw_frame().expect("draw worker transcript");
    assert!(cx.debug_bounds("subagent-progress").is_some());
    let transcript = cx
        .debug_bounds("subagent-transcript")
        .expect("subagent transcript");
    let row_bounds = cx
        .debug_bounds("agent-chat-row")
        .expect("centered agent chat row");
    assert!(
        row_bounds.size.width <= agent_appearance::COLUMN_WIDTH + px(1.0),
        "subagent row width {} should match COLUMN_WIDTH {}",
        row_bounds.size.width,
        agent_appearance::COLUMN_WIDTH
    );
    assert!(
        row_bounds.origin.x > transcript.origin.x + px(100.0),
        "subagent row should be centered inside wide transcript viewport"
    );
    let back = cx.debug_bounds("subagent-back").expect("back button");
    assert!(
        (back.origin.x - row_bounds.origin.x).abs() <= px(24.0),
        "subagent header back button ({}) should align with centered column ({})",
        back.origin.x,
        row_bounds.origin.x
    );
    assert!(
        cx.debug_bounds("tool-group-1").is_some(),
        "completed tools before assistant text should group like main agent UI"
    );
    assert!(
        cx.debug_bounds("tool-row-3").is_some(),
        "live running tool should stay visible like main agent UI"
    );

    cx.update_window_entity(&state, |state, _, cx| {
        let chat = &mut state.pane_mut(pane_id).unwrap().chat;
        let worker = chat.worker_view.as_mut().unwrap();
        for index in 0..40 {
            worker.messages.push(Arc::new(ChatMessage::new(
                ChatRole::Assistant,
                format!("Worker update {index}"),
            )));
        }
        chat.notify_worker_view(cx);
    });
    cx.draw_frame().unwrap();
    let follows = |cx: &gpui::VisualTestContext| {
        state.read_with(cx, |state, _| {
            let chat = &state.pane_ref(pane_id).unwrap().chat;
            assert!(
                chat.list_state.is_following_tail(),
                "worker scrolling must not move the parent"
            );
            chat.worker_view
                .as_ref()
                .unwrap()
                .list_state
                .is_following_tail()
        })
    };
    assert!(follows(&cx));
    for (delta, following) in [(300.0, false), (-10_000.0, true)] {
        cx.simulate_event(gpui::ScrollEvent {
            position: transcript.center(),
            delta: gpui::ScrollDelta::Pixels(point(px(0.0), px(delta))),
            ..Default::default()
        });
        cx.draw_frame().unwrap();
        cx.run_until_parked();
        cx.draw_frame().unwrap();
        assert_eq!(follows(&cx), following);
        assert_eq!(cx.debug_bounds("scroll-to-bottom-btn").is_none(), following);
    }
}

