fn measure_chat_frames(
    name: &str,
    pairs: usize,
    paragraphs: usize,
    deltas_per_frame: usize,
    live_paragraphs: usize,
) {
    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    let pane_id = state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);
    let mut reply =
        "A settled paragraph with **formatting** and a code snippet.\n\n".repeat(paragraphs);
    reply.push_str("```rust\nlet ready = true;\n```");
    let mut turn_trace = None;
    cx.update_window_entity(&state, |state, window, cx| {
        state.set_chat_mode(pane_id, true, window, cx);
        state.projects[0].sessions[0].status = SessionStatus::Working;
        let chat = &mut state.pane_mut(pane_id).unwrap().chat;
        chat.running = true;
        let messages = chat.visible_messages_mut();
        for _ in 0..pairs {
            messages.push(Arc::new(ChatMessage::new(ChatRole::User, "Prompt".into())));
            messages.push(Arc::new(ChatMessage::new(
                ChatRole::Assistant,
                reply.clone(),
            )));
            // Exercise the grouped-tool path in the visible window too.
            push_chat_event(
                messages,
                agent_bridge::AgentEvent::ToolEnd {
                    id: Some(format!("tool-{pairs}")),
                    name: "read".into(),
                    output: "ok".into(),
                    failed: false,
                },
            );
        }
        messages.push(Arc::new(ChatMessage::new(
            ChatRole::User,
            "Another prompt".into(),
        )));
        let turn = crate::stream_timing::new_trace();
        turn_trace = Some(turn);
        crate::stream_timing::turn_sent(turn, false);
        push_chat_event(
            messages,
            agent_bridge::AgentEvent::Text("Starting. ".into()),
        );
        for _ in 0..live_paragraphs {
            push_chat_event(
                messages,
                agent_bridge::AgentEvent::Text(
                    "A streamed paragraph with **formatting** and `code`.\n\n".into(),
                ),
            );
        }
        if let Some(last) = messages.last_mut() {
            Arc::make_mut(last).turn =
                crate::stream_timing::turn_started_at(turn).map(|started| (turn, started));
        }
        crate::stream_timing::turn_delta_arrived(turn, crate::stream_timing::DeltaKind::Answer);
        crate::stream_timing::turn_delta_applied(turn, crate::stream_timing::DeltaKind::Answer);
    });
    cx.update_window_entity(&state, |state, window, cx| {
        let input = state
            .pane_ref(pane_id)
            .unwrap()
            .input_state
            .clone()
            .unwrap();
        input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
    });
    for _ in 0..8 {
        cx.run_until_parked();
        cx.draw_commands();
    }
    let rss_before = process_rss_kib();
    let built_before = state.read_with(&cx, |state, _| {
        state
            .pane_ref(pane_id)
            .unwrap()
            .chat
            .row_cache
            .borrow()
            .built_rows
    });
    let mut samples = Vec::new();
    let mut input_samples = Vec::new();
    for frame in 0..48 {
        cx.update_window_entity(&state, |state, _, cx| {
            let chat = &mut state.pane_mut(pane_id).unwrap().chat;
            for _ in 0..deltas_per_frame {
                let text = if frame % 4 == 3 { "x \n\n" } else { "x " };
                let event = agent_bridge::AgentEvent::Text(text.into());
                chat.observe_status(&event);
                push_chat_event(&mut chat.messages, event);
            }
            chat.notify_content(cx);
        });
        let start = std::time::Instant::now();
        cx.draw_commands();
        samples.push(start.elapsed().as_micros());
        if frame % 8 == 0 {
            let start = std::time::Instant::now();
            cx.simulate_input("x");
            cx.draw_commands();
            input_samples.push(start.elapsed().as_micros());
        }
        cx.run_until_parked();
    }
    let built_rows = state.read_with(&cx, |state, _| {
        state
            .pane_ref(pane_id)
            .unwrap()
            .chat
            .row_cache
            .borrow()
            .built_rows
            - built_before
    });
    state.read_with(&cx, |state, cx| {
        let input = state
            .pane_ref(pane_id)
            .unwrap()
            .input_state
            .as_ref()
            .unwrap();
        assert_eq!(input.read(cx).value().as_str(), "xxxxxx");
    });
    input_samples.sort_unstable();
    samples.sort_unstable();
    if let Some(turn) = turn_trace {
        crate::stream_timing::turn_settled(turn);
    }
    println!(
        "{name}: messages={} cpu_scene_us p50={} p95={} max={} rss_delta_kib={} built_rows={built_rows}",
        pairs * 3 + 2,
        samples[24],
        samples[45],
        samples.last().copied().unwrap_or_default(),
        process_rss_kib().saturating_sub(rss_before)
    );
    println!(
        "{name}: input_to_scene_us p50={} p95={} max={}",
        input_samples[input_samples.len() / 2],
        input_samples[input_samples.len() - 1],
        input_samples.last().unwrap()
    );
    let capture =
        std::env::temp_dir().join(format!("shob-stream-{name}-{}.png", std::process::id()));
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        window.render_to_image().unwrap().save(&capture).unwrap();
        window.simulate_next_frame(cx);
    });
    println!("{name}: screenshot={}", capture.display());
    crate::stream_timing::report_now();
}

#[test]
#[ignore = "prints timings for the headless chat view"]
fn streaming_chat_frame_benchmark() {
    // Force the metadata timings this test prints. Safe here: the ignored
    // benchmark runs alone and this happens before the first frame.
    unsafe {
        std::env::set_var("SHOB_STREAM_TIMINGS", "1");
    }
    measure_chat_frames("short", 24, 16, 1, 0);
    measure_chat_frames("long", 240, 3, 1, 0);
    measure_chat_frames("long-bursty", 240, 3, 8, 0);
    measure_chat_frames("long-message", 1, 3, 1, 2_000);
}

#[test]
#[ignore = "real PTY output through headless GPUI; enable SHOB_DIAGNOSTICS"]
fn terminal_stream_frame_benchmark() {
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.draw_frame().unwrap();
    // Exclude font/shader/session startup from steady-stream samples.
    for _ in 0..8 {
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(16));
        cx.draw_frame().unwrap();
    }
    let id = state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);
    let mut sampler = diagnostics::Sampler::new();
    let _ = sampler.report(); // reset startup counters and CPU baseline
    let started = std::time::Instant::now();
    cx.update_window_entity(&state, |state, window, cx| {
            let input = state.pane_ref(id).unwrap().input_state.clone().unwrap();
            input.update(cx, |input, cx| input.set_value(
                "bash -c 'for ((i=0;i<20000;i++)); do printf \"\\033[31mstream line %s\\033[0m\\n\" \"$i\"; done; printf \"SHOB_STREAM_%s\\n\" END'",
                window, cx,
            ));
            state.execute_pane_input(id, window, cx);
        });
    // Paint while input receipts and output are in flight. Waiting for
    // submission first would manufacture a foreground-consumer stall.
    let mut frames = Vec::new();
    loop {
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(16));
        let frame = std::time::Instant::now();
        cx.draw_frame().unwrap();
        frames.push(frame.elapsed().as_micros());
        let complete = state.read_with(&cx, |state, cx| {
            state
                .pane_ref(id)
                .unwrap()
                .terminal
                .read(cx)
                .bottom_lines(8)
                .iter()
                .any(|line| line.trim() == "SHOB_STREAM_END")
        });
        if complete {
            break;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(15),
            "PTY output did not finish"
        );
        std::thread::yield_now();
    }
    frames.sort_unstable();
    println!(
        "pty_ansi_lines=20000 elapsed_ms={} frames={} frame_us_p50={} frame_us_max={}",
        started.elapsed().as_millis(),
        frames.len(),
        frames[frames.len() / 2],
        frames.last().unwrap()
    );
    println!("{}", sampler.report());
}

#[test]
fn batch_tool_row_stays_visible_after_completion() {
    use agent_bridge::AgentEvent;

    let mut test_app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut test_app);
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, window, cx| {
            s.set_chat_mode(pane_id, true, window, cx);
            let messages = s.pane_mut(pane_id).unwrap().chat.visible_messages_mut();
            push_chat_event(
                messages,
                AgentEvent::ToolStart {
                    id: Some("batch-1".into()),
                    name: "batch".into(),
                    input: r#"{"calls":[{"tool":"read","path":"README.md"},{"tool":"grep","path":"src","pattern":"TODO"},{"tool":"read_file","path":"Cargo.toml"}]}"#.into(),
                },
            );
            assert_eq!(chat_activity(messages), "Read, Ripgrep · 3 calls…");
            assert_eq!(chat_batch::counts(messages[0].tool.as_ref().unwrap()).as_str(), "Read 2 · Ripgrep 1");
            let activity = chat_activity_tooltip(messages);
            assert!(activity.contains("Read · README.md"));
            assert!(activity.contains("Ripgrep · src"));
            assert!(activity.contains("TODO"));
            cx.notify();
        });
    cx.draw_frame().unwrap();
    assert!(
        cx.debug_bounds("tool-row-0").is_some(),
        "Batch group is visible"
    );
    assert!(
        cx.debug_bounds("tool-call-0-0").is_some(),
        "running calls start open"
    );
    assert!(
        cx.debug_bounds("tool-call-0-2").is_some(),
        "every call is visible"
    );
    cx.update_window_entity(&state, |s, _, cx| {
            let messages = s.pane_mut(pane_id).unwrap().chat.visible_messages_mut();
            push_chat_event(
                messages,
                AgentEvent::ToolEnd {
                    id: Some("batch-1".into()),
                    name: "batch".into(),
                    output: r#"[{"index":0,"tool":"read","is_error":false,"result":{"content":[{"type":"text","text":"Shob\n  **literal** <tag>"}]}},{"index":1,"tool":"grep","is_error":true,"error":"Permission denied"},{"index":2,"tool":"read_file","is_error":false,"result":{"content":[{"type":"text","text":"[package]\nname = \"shob\""}]}}]"#.into(),
                    failed: false,
                },
            );
            push_chat_event(messages, AgentEvent::Text("I read the README.".into()));
            push_chat_event(messages, AgentEvent::Done);
            cx.notify();
        });
    cx.draw_frame().unwrap();
    let group = cx
        .debug_bounds("tool-row-0")
        .expect("completed Batch summary stays visible");
    assert!(
        cx.debug_bounds("tool-call-0-0").is_none(),
        "completion collapses the group"
    );
    assert!(cx.debug_bounds("tool-call-0-1").is_none());
    assert!(cx.debug_bounds("tool-group-0").is_none());
    if let Some(path) = std::env::var_os("SHOB_BENCH_SCREENSHOT") {
        let path = PathBuf::from(path).with_extension("collapsed.png");
        cx.update(|window, _| window.render_to_image().unwrap().save(path).unwrap());
    }
    cx.simulate_click(group.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    let first = cx
        .debug_bounds("tool-call-0-0")
        .expect("Read stays visible");
    assert!(
        cx.debug_bounds("tool-call-0-1").is_some(),
        "Ripgrep stays visible"
    );
    assert!(cx.debug_bounds("tool-call-output-0-0").is_none());
    assert!(cx.debug_bounds("tool-call-output-0-1").is_none());
    cx.simulate_click(first.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("tool-call-output-0-0").is_some());
    assert!(cx.debug_bounds("tool-call-output-0-1").is_none());
    let second = cx.debug_bounds("tool-call-0-1").unwrap();
    cx.simulate_click(second.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("tool-call-output-0-0").is_some());
    assert!(cx.debug_bounds("tool-call-output-0-1").is_some());
    if let Some(path) = std::env::var_os("SHOB_BENCH_SCREENSHOT") {
        cx.update(|window, _| window.render_to_image().unwrap().save(path).unwrap());
    }
    let first = cx.debug_bounds("tool-call-0-0").unwrap();
    cx.simulate_click(first.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("tool-call-output-0-0").is_none());
    assert!(cx.debug_bounds("tool-call-output-0-1").is_some());
}

#[test]
#[ignore = "prints timings for headless tool cards and output"]
fn tool_feedback_frame_benchmark() {
    use crate::stream_timing as timing;
    use agent_bridge::AgentEvent;

    let mut cx = TestAppContext::new();
    let (mut cx, state) = open_app(&mut cx);
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |s, window, cx| {
        s.set_chat_mode(pane_id, true, window, cx);
    });
    cx.draw_frame().unwrap();
    for sample in 0..20 {
        let id = format!("feedback-{sample}");
        timing::tool_arrived(&id);
        cx.update_window_entity(&state, |s, _, cx| {
            let messages = s.pane_mut(pane_id).unwrap().chat.visible_messages_mut();
            messages.clear();
            push_chat_event(
                messages,
                AgentEvent::ToolStart {
                    id: Some(id.clone()),
                    name: "bash".into(),
                    input: r#"{"command":"printf ready"}"#.into(),
                },
            );
            Arc::make_mut(messages.last_mut().unwrap())
                .tool
                .as_mut()
                .unwrap()
                .expanded = true;
            cx.notify();
        });
        cx.draw_frame().unwrap();
        assert!(cx.debug_bounds("tool-row-0").is_some());
        cx.update_window_entity(&state, |s, _, cx| {
            timing::tool_output_arrived(&id);
            push_chat_event(
                s.pane_mut(pane_id).unwrap().chat.visible_messages_mut(),
                AgentEvent::ToolUpdate {
                    id: Some(id.clone()),
                    name: "bash".into(),
                    output: "ready".into(),
                },
            );
            cx.notify();
        });
        cx.draw_frame().unwrap();
        if sample == 0
            && let Some(path) = std::env::var_os("SHOB_BENCH_SCREENSHOT")
        {
            cx.update(|window, _| {
                window
                    .render_to_image()
                    .expect("capture tool card")
                    .save(path)
                    .expect("save tool card");
            });
        }
        timing::tool_finished(&id);
        cx.update_window_entity(&state, |s, _, cx| {
            let messages = s.pane_mut(pane_id).unwrap().chat.visible_messages_mut();
            push_chat_event(
                messages,
                AgentEvent::ToolEnd {
                    id: Some(id),
                    name: "bash".into(),
                    output: "ready".into(),
                    failed: false,
                },
            );
            cx.notify();
        });
        cx.draw_frame().unwrap();
    }
    timing::report_now();
}

#[test]
#[ignore = "requires two local provider fixtures"]
fn loopback_provider_switch_keeps_context() {
    use agent_bridge::AgentEvent;
    use std::time::Duration;

    let cwd = PathBuf::from(std::env::var_os("SHOB_BENCH_CWD").unwrap());
    let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel(1);
    agent_bridge::set_wake(Arc::new(move || {
        let _ = wake_tx.try_send(());
    }));
    let mut chat = PaneChat::default();
    let mut session_id = None;
    for (provider, model, prompt) in [
        ("bench", "fixture", "Remember the switch fixture."),
        (
            "bench-second",
            "switched",
            "Check switched provider history.",
        ),
    ] {
        chat.selected_model = Some(agent_auth::AgentModel {
            provider: provider.into(),
            id: model.into(),
            backend: agent_auth::ModelBackend::Embedded,
        });
        chat.ensure_agent(cwd.clone()).unwrap();
        let context = chat.context_prompt(prompt);
        let bridge = chat.bridge.as_mut().unwrap();
        bridge
            .send_prompt_with_context(prompt, chat.bridge_resume.as_deref(), context)
            .unwrap();
        chat.messages
            .push(Arc::new(ChatMessage::new(ChatRole::User, prompt.into())));
        let mut answer = String::new();
        'turn: loop {
            wake_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            for event in bridge.poll() {
                match &event {
                    AgentEvent::Text(text) => answer.push_str(text),
                    AgentEvent::Done => break 'turn,
                    AgentEvent::Error(error) => panic!("{error}"),
                    _ => {}
                }
                push_chat_event(&mut chat.messages, event);
            }
        }
        assert_eq!(answer, "x".repeat(16));
        finish_chat_streaming(&mut chat.messages);
        let actual_id = bridge.session_id().expect("durable shob session");
        if let Some(expected) = &session_id {
            assert_eq!(&actual_id, expected);
        }
        chat.shob_session_id = Some(actual_id.clone());
        session_id = Some(actual_id);
    }
    assert_eq!(chat.messages.len(), 4);
}

#[test]
#[ignore = "requires the local streaming benchmark endpoint"]
fn loopback_tool_latency_benchmark() {
    use crate::stream_timing as timing;
    use agent_bridge::{AgentBridge, AgentEvent};
    use std::time::Duration;

    embedded::set_tool_arrival_hook(timing::tool_arrived);
    embedded::set_tool_end_hook(timing::tool_finished);
    embedded::set_tool_output_hook(timing::tool_output_arrived);
    let cwd = PathBuf::from(std::env::var_os("SHOB_BENCH_CWD").unwrap());
    let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel(1);
    agent_bridge::set_wake(Arc::new(move || {
        let _ = wake_tx.try_send(());
    }));
    let mut bridge = AgentBridge::spawn_model(&cwd, Some("bench/fixture")).unwrap();
    let trace = bridge.trace_id();
    for sample in 0..20 {
        timing::turn_sent(trace, sample > 0);
        bridge.send_prompt("Run two benchmark tools.").unwrap();
        let mut results = std::collections::HashMap::new();
        let mut answer = String::new();
        'turn: loop {
            wake_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            for request in bridge.poll_requests() {
                assert!(request.approval && request.title == "Allow bash?");
                request
                    .reply
                    .try_send(crate::agent_interaction::Response::AllowOnce)
                    .unwrap();
            }
            for event in bridge.poll() {
                match event {
                    AgentEvent::ToolEnd {
                        name,
                        output,
                        failed,
                        ..
                    } => {
                        assert!(!failed, "{name}: {output}");
                        results.insert(name, output);
                    }
                    AgentEvent::Text(text) => answer.push_str(&text),
                    AgentEvent::Done => break 'turn,
                    AgentEvent::Error(error) => panic!("{error}"),
                    _ => {}
                }
            }
        }
        assert_eq!(results.len(), 2);
        assert!(results["read"].contains("benchmark fixture"));
        assert_eq!(results["bash"].trim(), "ready");
        assert_eq!(answer, "x".repeat(16));
        timing::turn_settled(trace);
    }
    timing::report_now();
}

#[test]
#[ignore = "requires an authorized Antigravity test account in the embedded credential store"]
fn live_native_antigravity_smoke() {
    use agent_bridge::AgentEvent;
    use std::time::{Duration, Instant};

    let cwd = std::env::current_dir().unwrap();
    let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel(1);
    agent_bridge::set_wake(Arc::new(move || {
        let _ = wake_tx.try_send(());
    }));
    let start = Instant::now();
    let mut bridge =
        agent_bridge::AgentBridge::spawn_model(&cwd, Some("google-antigravity/gemini-3.8-flash"))
            .unwrap();
    bridge.set_thinking_level("low").unwrap();
    crate::stream_timing::turn_sent(bridge.trace_id(), false);
    bridge
        .send_prompt("Reply with exactly OK. Answer directly without tools.")
        .unwrap();
    let mut answer = String::new();
    let mut first = None;
    'turn: loop {
        let remaining = Duration::from_secs(60)
            .checked_sub(start.elapsed())
            .expect("Antigravity response timed out");
        wake_rx.recv_timeout(remaining).unwrap();
        for event in bridge.poll() {
            match event {
                AgentEvent::Text(text) => {
                    first.get_or_insert_with(|| start.elapsed());
                    answer.push_str(&text);
                }
                AgentEvent::Done => break 'turn,
                AgentEvent::Error(error) => panic!("{error}"),
                _ => {}
            }
        }
    }
    assert_eq!(answer.trim(), "OK");
    println!(
        "Antigravity first_ms={} total_ms={}",
        first.unwrap().as_millis(),
        start.elapsed().as_millis()
    );
    crate::stream_timing::report_now();
}

#[test]
#[ignore = "requires scripts/bench-agent-stream.py local Gemini fixture"]
fn loopback_gemini_tool_round_trip() {
    use agent_bridge::AgentEvent;
    use std::time::{Duration, Instant};

    let cwd = PathBuf::from(std::env::var_os("SHOB_BENCH_CWD").unwrap());
    let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel(1);
    agent_bridge::set_wake(Arc::new(move || {
        let _ = wake_tx.try_send(());
    }));
    let mut chat = PaneChat {
        selected_model: Some(agent_auth::AgentModel {
            provider: "google".into(),
            id: "gemini-3.8-flash".into(),
            backend: agent_auth::ModelBackend::Embedded,
        }),
        ..Default::default()
    };
    for phase in ["cold", "warm"] {
        let started = Instant::now();
        chat.ensure_agent(cwd.clone()).unwrap();
        let bridge = chat.bridge.as_mut().unwrap();
        bridge.send_prompt("Run two benchmark tools.").unwrap();
        let mut first_answer = None;
        let mut reads = Vec::new();
        let mut answer = String::new();
        'turn: loop {
            assert!(started.elapsed() < Duration::from_secs(15));
            wake_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            for event in bridge.poll() {
                match event {
                    AgentEvent::ToolEnd {
                        name,
                        output,
                        failed,
                        ..
                    } => {
                        assert_eq!(name, "read");
                        assert!(!failed, "{output}");
                        reads.push(output);
                    }
                    AgentEvent::Text(text) => {
                        first_answer.get_or_insert_with(|| started.elapsed());
                        answer.push_str(&text);
                    }
                    AgentEvent::Done => break 'turn,
                    AgentEvent::Error(error) => panic!("{error}"),
                    _ => {}
                }
            }
        }
        assert_eq!(reads.len(), 2);
        assert!(reads.iter().any(|read| read.contains("113")));
        assert!(reads.iter().any(|read| read.contains("127")));
        assert_eq!(answer, "x".repeat(16));
        println!(
            "gemini {phase} first_us={} total_us={}",
            first_answer.unwrap().as_micros(),
            started.elapsed().as_micros()
        );
    }
}

#[test]
#[ignore = "renders deterministic agent chat comparison images"]
fn agent_chat_appearance_capture() {
    let output = std::env::var("SHOB_CHAT_CAPTURE_DIR").expect("SHOB_CHAT_CAPTURE_DIR");
    std::fs::create_dir_all(&output).unwrap();
    for (width, height) in [(819., 684.), (955., 955.), (420., 800.)] {
        for dark in [true, false] {
            let mut app = TestAppContext::new();
            let (mut cx, state) = open_app(&mut app);
            cx.simulate_resize(gpui::size(px(width), px(height)));
            let pane_id = state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);
            cx.update_window_entity(&state, |state, window, cx| {
                state.settings.agent_layout = true;
                state.sidebar_visible = false;
                state.theme_preference = if dark { ThemePreference::Dark } else { ThemePreference::Light };
                theme::apply_component_theme(if dark { gpui_component::ThemeMode::Dark } else { gpui_component::ThemeMode::Light }, Some(window), cx);
                state.set_chat_mode(pane_id, true, window, cx);
                let chat = &mut state.pane_mut(pane_id).unwrap().chat;
                chat.messages = vec![
                    Arc::new(ChatMessage::new(ChatRole::User, "Make the agent chat readable and consistent.".into())),
                    Arc::new(ChatMessage::new(ChatRole::Assistant, "Implemented a shared **Markdown appearance** with readable spacing and `inline code`. User messages preserve line breaks, paths, and ~~strikethrough~~.\n\n### How Shob runs\n\nThe renderer uses `with_frame_cell_geometry` and `request_render_using_scroll_content()` to keep updates responsive when a long inline code span wraps onto another line.\n\n- **Native text** keeps selection and copying.\n- Nested lists stay aligned:\n  - Links open [source files](src/main.rs) or [documentation](https://example.com).\n- [x] Keep completed messages stable\n\n```rust\nlet ready = true;\nprintln!(\"Shob is ready\");\n```\n\n> Background parsing keeps the interface responsive.\n\n| Surface | Result |\n| :--- | ---: |\n| Markdown | Ready |\n| Streaming | Ready |".into())),
                ];
                let mut answer = chat.messages.pop().unwrap();
                Arc::make_mut(&mut answer).source_timestamp = Some(3_500);
                push_chat_event_at(&mut chat.messages, agent_bridge::AgentEvent::Thinking("Checking the renderer and the composer layout.".into()), Some(1_000));
                push_chat_event_at(&mut chat.messages, agent_bridge::AgentEvent::ToolStart { id: Some("appearance-read".into()), name: "read".into(), input: r#"{"path":"src/main.rs"}"#.into() }, Some(1_500));
                push_chat_event_at(&mut chat.messages, agent_bridge::AgentEvent::ToolEnd { id: Some("appearance-read".into()), name: "read".into(), output: "Read the agent chat entry point.".into(), failed: false }, Some(2_000));
                chat.messages.push(answer);
                Arc::make_mut(&mut chat.messages[0]).source_timestamp = Some(1_000);
                finish_worked_turn(&mut chat.messages, Some(861_000));
                chat.info_open = Some(false);
                chat.notify_content(cx);
            });
            for _ in 0..12 { cx.run_until_parked(); cx.draw_commands(); }
            let path = PathBuf::from(&output).join(format!("chat-{}x{}-{}.png", width as u32, height as u32, if dark { "dark" } else { "light" }));
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                window.render_to_image().unwrap().save(&path).unwrap();
                window.simulate_next_frame(cx);
            });
            println!("capture={}", path.display());
        }
    }
}

#[test]
#[ignore = "renders the exact supplied Codex Markdown reference"]
fn codex_markdown_reference_capture() {
    struct ReferenceView { owner: Entity<AppState>, pane_id: usize }
    impl Render for ReferenceView {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.owner.update(cx, |owner, cx| {
                let pane = owner.pane_ref(self.pane_id).unwrap();
                let tokens = owner.tokens(cx);
                let rows = AppState::build_chat_rows(&pane.chat, false);
                let count = rows.borrow().rows.len();
                let snapshot = ChatSnapshot {
                    focused_layout: true, pane_id: self.pane_id,
                    conversation: pane.chat.conversation_key.clone().into(), tokens, rows,
                    unready_row: Default::default(),
                    list_state: None,
                    preserve_anchor: false,
                    host: UiHost::new(cx.weak_entity()),
                };
                div().size_full().overflow_hidden().bg(tokens.chat().background)
                    .font_family(cx.theme().font_family.clone()).pt(px(28.))
                    .children((0..count).map(|index| AppState::render_chat_row(&snapshot, index, window, cx)).collect::<Vec<_>>())
            })
        }
    }
    let output = std::env::var("SHOB_CHAT_CAPTURE_DIR").expect("SHOB_CHAT_CAPTURE_DIR");
    std::fs::create_dir_all(&output).unwrap();
    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    cx.simulate_resize(gpui::size(px(819.), px(684.)));
    let pane_id = state.read_with(&cx, |state, _| state.projects[0].sessions[0].panes[0].id);
    cx.update_window_entity(&state, |state, window, cx| {
        state.theme_preference = ThemePreference::Dark;
        state.apply_theme_preference(Some(window), cx);
        state.set_chat_mode(pane_id, true, window, cx);
        let chat = &mut state.pane_mut(pane_id).unwrap().chat;
        let mut thinking = ChatMessage::new(ChatRole::Thinking, "Inspecting the formatter.".into());
        thinking.source_timestamp = Some(1_000);
        thinking.worked_ms = Some(279_000);
        thinking.worked_end = 2;
        thinking.worked_answer = Some(1);
        let mut answer = ChatMessage::new(ChatRole::Assistant, "Implemented an initial agent Markdown formatter in [agent_markdown.rs](/home/nazmul/Desktop/shob/components/src/text/format/agent_markdown.rs) and connected it to the GPUI chat. User messages now render Markdown, preserve line breaks and literal HTML, and keep single tildes in paths while supporting `~~strikethrough~~`. Assistant messages use the new formatter entry point and retain the existing background streaming pipeline.\n\nFile-link chips, GitHub alerts, Mermaid, math, and media interactions are still pending.\n\n`cargo check --bin shob`, `cargo fmt --check`, and the focused streaming test passed. The new parser-specific test could not run: testing the component crate standalone resolves an incompatible GPUI version.".into());
        answer.source_timestamp = Some(280_000);
        chat.messages = vec![Arc::new(thinking), Arc::new(answer)];
    });
    let preview = cx.new(|_| ReferenceView { owner: state, pane_id });
    cx.replace_root_view(|window, cx| Root::new(preview, window, cx));
    for _ in 0..12 { cx.run_until_parked(); cx.draw_commands(); }
    let path = PathBuf::from(output).join("codex-reference-shob.png");
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        window.render_to_image().unwrap().save(&path).unwrap();
        window.simulate_next_frame(cx);
    });
    println!("capture={}", path.display());
}
