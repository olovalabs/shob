#[test]
#[ignore = "requires the local streaming benchmark endpoint"]
fn loopback_agent_stream_benchmark() {
    use agent_bridge::{AgentBridge, AgentEvent};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let cwd = PathBuf::from(std::env::var_os("SHOB_BENCH_CWD").unwrap());
    let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel(1);
    agent_bridge::set_wake(Arc::new(move || {
        let _ = wake_tx.try_send(());
    }));
    unsafe {
        std::env::set_var("SHOB_STREAM_TIMINGS", "1");
    }
    let settle = std::env::var_os("SHOB_BENCH_SETTLE").is_some();
    let mut resumed_session: Option<String> = None;
    for sample in 0..12 {
        let startup = Instant::now();
        let mut bridge = match resumed_session.as_deref() {
            Some(session) => {
                AgentBridge::spawn_resumed(&cwd, Some(session), Some("bench/fixture")).unwrap()
            }
            None => AgentBridge::spawn_model(&cwd, Some("bench/fixture")).unwrap(),
        };
        let spawn = startup.elapsed();
        let mut messages = Vec::new();
        for turn in 0..2 {
            let start = Instant::now();
            let trace = bridge.trace_id();
            crate::stream_timing::turn_sent(trace, turn > 0);
            bridge.send_prompt("Say x sixteen times.").unwrap();
            let mut first = None;
            let mut last = None;
            let mut max_gap = Duration::ZERO;
            let mut count = 0;
            let mut seen = String::new();
            let mut done = false;
            let mut completions = 0;
            while !done {
                wake_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                for event in bridge.poll() {
                    match &event {
                        AgentEvent::Error(error) => panic!("{error}"),
                        AgentEvent::Text(text) => {
                            count += text.len();
                            seen.push_str(text);
                            let now = start.elapsed();
                            first.get_or_insert(now);
                            if let Some(previous) = last {
                                max_gap = max_gap.max(now - previous);
                            }
                            last = Some(now);
                            crate::stream_timing::turn_delta_applied(
                                trace,
                                crate::stream_timing::DeltaKind::Answer,
                            );
                        }
                        AgentEvent::Done => {
                            completions += 1;
                            done = true;
                            crate::stream_timing::turn_settled(trace);
                            finish_chat_streaming(&mut messages);
                        }
                        _ => {}
                    }
                    push_chat_event(&mut messages, event);
                }
            }
            assert_eq!(count, 16);
            assert_eq!(seen, "x".repeat(16), "exact transcript text and order");
            if !settle {
                assert_eq!(completions, 1);
            }
            println!(
                "sample={sample} turn={turn} spawn_us={} ttft_us={} last_us={} total_us={} max_gap_us={} completions={completions}",
                spawn.as_micros(),
                first.unwrap().as_micros(),
                last.unwrap().as_micros(),
                start.elapsed().as_micros(),
                max_gap.as_micros(),
            );
            if settle {
                std::thread::sleep(Duration::from_millis(15));
                bridge.poll();
            }
        }
        if let Some(id) = bridge.session_id() {
            resumed_session = Some(id);
        }
        let drop_start = Instant::now();
        drop(bridge);
        println!("drop_us={}", drop_start.elapsed().as_micros());
    }
    crate::stream_timing::report_now();

    // Bursty stream: the fixture writes 512 tokens without pacing. Warm
    // turns must deliver the first delta within a frame of the request.
    const BURST_TOKENS: usize = 512;
    for sample in 0..8 {
        let startup = Instant::now();
        let mut bridge = AgentBridge::spawn_model(&cwd, Some("bench/fixture")).unwrap();
        let spawn = startup.elapsed();
        let mut messages = Vec::new();
        for turn in 0..3 {
            let start = Instant::now();
            let trace = bridge.trace_id();
            crate::stream_timing::turn_sent(trace, turn > 0);
            bridge.send_prompt("Burst 512 tokens.").unwrap();
            let mut first = None;
            let mut last = None;
            let mut max_gap = Duration::ZERO;
            let mut count = 0;
            let mut seen = String::new();
            let mut done = false;
            let mut completions = 0;
            while !done {
                wake_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                for event in bridge.poll() {
                    match &event {
                        AgentEvent::Error(error) => panic!("{error}"),
                        AgentEvent::Text(text) => {
                            count += text.len();
                            seen.push_str(text);
                            let now = start.elapsed();
                            first.get_or_insert(now);
                            if let Some(previous) = last {
                                max_gap = max_gap.max(now - previous);
                            }
                            last = Some(now);
                            crate::stream_timing::turn_delta_applied(
                                trace,
                                crate::stream_timing::DeltaKind::Answer,
                            );
                        }
                        AgentEvent::Done => {
                            completions += 1;
                            done = true;
                            crate::stream_timing::turn_settled(trace);
                            finish_chat_streaming(&mut messages);
                        }
                        _ => {}
                    }
                    push_chat_event(&mut messages, event);
                }
            }
            assert_eq!(count, BURST_TOKENS);
            assert_eq!(seen, "x".repeat(BURST_TOKENS));
            assert_eq!(completions, 1, "one completion per turn");
            println!(
                "burst sample={sample} turn={turn} spawn_us={} ttft_us={} last_us={} total_us={} max_gap_us={}",
                spawn.as_micros(),
                first.unwrap().as_micros(),
                last.unwrap().as_micros(),
                start.elapsed().as_micros(),
                max_gap.as_micros(),
            );
        }
        drop(bridge);
    }
    crate::stream_timing::report_now();

    // Cancellation: stream a paced answer, stop after the first delta, and
    // verify one completion plus a settled turn.
    let mut bridge = AgentBridge::spawn_model(&cwd, Some("bench/fixture")).unwrap();
    let mut messages = Vec::new();
    let trace = bridge.trace_id();
    crate::stream_timing::turn_sent(trace, true);
    bridge.send_prompt("Slow 200 tokens.").unwrap();
    let mut first_delta = None;
    let mut cancel_at = None;
    let mut done = false;
    let mut completions = 0;
    while !done {
        wake_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        for event in bridge.poll() {
            match &event {
                AgentEvent::Text(_) => {
                    first_delta.get_or_insert_with(Instant::now);
                    if cancel_at.is_none() {
                        crate::stream_timing::turn_cancelled(trace);
                        cancel_at = Some(Instant::now());
                        bridge.cancel().unwrap();
                    }
                }
                AgentEvent::Done => {
                    completions += 1;
                    done = true;
                    crate::stream_timing::turn_settled(trace);
                    finish_chat_streaming(&mut messages);
                }
                AgentEvent::Error(error) => {
                    // An aborted provider request may surface as an error.
                    let lower = error.to_ascii_lowercase();
                    assert!(
                        lower.contains("abort") || lower.contains("cancel"),
                        "unexpected error during cancel: {error}"
                    );
                }
                _ => {}
            }
            push_chat_event(&mut messages, event);
        }
    }
    assert_eq!(completions, 1, "cancellation must settle exactly once");
    let cancel_at = cancel_at.expect("a delta arrived before cancel");
    println!(
        "cancel first_delta_to_cancel_us={} settle_us={}",
        first_delta
            .expect("first delta")
            .duration_since(cancel_at)
            .as_micros(),
        cancel_at.elapsed().as_micros()
    );
    drop(bridge);
    crate::stream_timing::report_now();

    // Two panes streaming at once must both complete exactly once without
    // blocking the shared pump thread.
    fn process_stats() -> (u64, u64) {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let read = |key: &str| {
            status
                .lines()
                .find(|line| line.starts_with(key))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0)
        };
        (read("Threads:"), read("VmRSS:"))
    }
    let (threads_before, _) = process_stats();
    let mut first = AgentBridge::spawn_model(&cwd, Some("bench/fixture")).unwrap();
    let mut second = AgentBridge::spawn_model(&cwd, Some("bench/fixture")).unwrap();
    let traces = [first.trace_id(), second.trace_id()];
    for trace in traces {
        crate::stream_timing::turn_sent(trace, false);
    }
    first.send_prompt("Say x sixteen times.").unwrap();
    second.send_prompt("Say x sixteen times.").unwrap();
    let mut counts = [0usize; 2];
    let mut completions = [0usize; 2];
    let mut done = [false; 2];
    while !done.iter().all(|done| *done) {
        wake_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        for (index, bridge) in [&mut first, &mut second].into_iter().enumerate() {
            for event in bridge.poll() {
                match &event {
                    AgentEvent::Text(text) => counts[index] += text.len(),
                    AgentEvent::Done => {
                        completions[index] += 1;
                        done[index] = true;
                        crate::stream_timing::turn_settled(traces[index]);
                    }
                    AgentEvent::Error(error) => panic!("{error}"),
                    _ => {}
                }
            }
        }
    }
    assert_eq!(counts, [16, 16]);
    assert_eq!(completions, [1, 1], "one completion per concurrent pane");
    let (threads_after, rss_kib) = process_stats();
    println!(
        "concurrent threads before={threads_before} after={threads_after} rss_kib={rss_kib} counts={counts:?} completions={completions:?}"
    );
    drop(first);
    drop(second);

    // Backend failure: one Error and one Done, then the same bridge must
    // accept an immediate follow-up successfully.
    let mut bridge = AgentBridge::spawn_model(&cwd, Some("bench/fixture")).unwrap();
    let mut messages = Vec::new();
    let trace = bridge.trace_id();
    crate::stream_timing::turn_sent(trace, false);
    bridge.send_prompt("Fail request.").unwrap();
    let mut errors = 0;
    let mut completions = 0;
    let mut done = false;
    while !done {
        wake_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        for event in bridge.poll() {
            match &event {
                AgentEvent::Error(error) => {
                    let lower = error.to_ascii_lowercase();
                    assert!(lower.contains("400") || lower.contains("boom"), "{error}");
                    errors += 1;
                }
                AgentEvent::Done => {
                    completions += 1;
                    done = true;
                    crate::stream_timing::turn_settled(trace);
                }
                _ => {}
            }
            push_chat_event(&mut messages, event);
        }
    }
    assert_eq!(errors, 1);
    assert_eq!(completions, 1);
    crate::stream_timing::turn_sent(trace, true);
    bridge.send_prompt("Say x sixteen times.").unwrap();
    let mut follow_count = 0;
    let mut follow_done = false;
    let mut follow_completions = 0;
    while !follow_done {
        wake_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        for event in bridge.poll() {
            match &event {
                AgentEvent::Text(text) => follow_count += text.len(),
                AgentEvent::Done => {
                    follow_completions += 1;
                    follow_done = true;
                    crate::stream_timing::turn_settled(trace);
                }
                AgentEvent::Error(error) => panic!("follow-up failed: {error}"),
                _ => {}
            }
        }
    }
    assert_eq!(follow_count, 16);
    assert_eq!(follow_completions, 1);
    println!("backend_failure errors={errors} completions={completions} follow_up=ok");
    drop(bridge);
}

#[test]
#[ignore = "prints slow-reader enqueue timings; run with --ignored --nocapture"]
fn slow_reader_writer_benchmark() {
    use crate::agent_writer::AgentWriter;
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    struct SlowReader {
        release: Arc<AtomicBool>,
    }

    impl Write for SlowReader {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            while !self.release.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let release = Arc::new(AtomicBool::new(false));
    let sink_release = Arc::clone(&release);
    let writer = AgentWriter::spawn(
        SlowReader {
            release: sink_release,
        },
        |error| {
            eprintln!("slow-reader write error: {error}");
        },
    );
    let prompt = "x".repeat(4 * 1024 * 1024);
    let start = Instant::now();
    writer.send_prompt("slow".into(), prompt).unwrap();
    let prompt_us = start.elapsed().as_micros();
    let mut worst = Duration::ZERO;
    for index in 0..2_000 {
        let start = Instant::now();
        writer
            .send_json(serde_json::json!({"type":"abort","id":index}))
            .unwrap();
        worst = worst.max(start.elapsed());
    }
    let queue_us = start.elapsed().as_micros();
    println!(
        "slow_reader prompt_enqueue_us={prompt_us} 2000_abort_enqueue_us={queue_us} worst_us={}",
        worst.as_micros()
    );
    release.store(true, Ordering::Release);
    writer.shutdown();
}

#[test]
#[ignore = "prints long-block streaming costs; run with --ignored --nocapture"]
fn streaming_unsettled_block_microbench() {
    use std::hint::black_box;
    use std::time::Instant;

    for (name, opener, delta) in [
        ("fence", "```rust\n", "let value = \"世界🦀\";\n"),
        ("list", "7. first\n", "8. next\n   - nested 世界🦀\n"),
        ("unfinished-line", "```text\n", "世界🦀"),
    ] {
        for deltas in [2_000, 8_000] {
            let initial = format!("{opener}{}", delta.repeat(1_000));
            let mut legacy = initial.clone();
            let mut legacy_bytes = 0usize;
            let start = Instant::now();
            for _ in 0..deltas {
                legacy.push_str(delta);
                // Lower bound on the old full-tail scan/copy: omit Markdown
                // parsing, highlighting and list/fence classification.
                black_box(legacy.lines().count());
                legacy_bytes += legacy.len();
                black_box(SharedString::from(legacy.as_str()));
            }
            let legacy_time = start.elapsed();

            let mut message = ChatMessage::new(ChatRole::Assistant, initial);
            refresh_stream_view(&mut message, true);
            let mut preview_bytes = 0usize;
            let mut parser_bytes = 0usize;
            let mut max_preview = 0usize;
            let mut samples = Vec::with_capacity(deltas);
            let start = Instant::now();
            for _ in 0..deltas {
                let tick = Instant::now();
                message.text.push_str(delta);
                refresh_stream_view(&mut message, delta.contains('\n'));
                samples.push(tick.elapsed().as_nanos());
                let view = message.stream.as_ref().unwrap();
                preview_bytes += view.tail.len();
                max_preview = max_preview.max(view.tail.len());
                if !view.plain_tail {
                    parser_bytes += view.tail.len();
                }
                black_box(view.tail.clone());
            }
            let bounded_time = start.elapsed();
            assert_eq!(message.text, legacy);
            assert!(max_preview <= STREAM_LIVE_BYTES);
            assert_eq!(parser_bytes, 0, "oversized blocks bypass the live parser");
            samples.sort_unstable();
            println!(
                "unsettled {name} deltas={deltas} final_bytes={} legacy_scan_copy_lower_bound={legacy_time:?} bounded_refresh={bounded_time:?} refresh_ns_p50={} p95={} legacy_live_parser_and_copy_bytes={legacy_bytes} bounded_preview_copy_bytes={preview_bytes} bounded_live_parser_bytes={parser_bytes} max_preview_bytes={max_preview}",
                message.text.len(),
                samples[deltas / 2],
                samples[deltas * 95 / 100],
            );
        }
    }
}

#[test]
#[ignore = "prints microbenchmark timings; run with --ignored --nocapture"]
fn streaming_hot_path_microbench() {
    use agent_bridge::AgentEvent;
    use std::hint::black_box;
    use std::time::Instant;

    // Legacy whole-prefix scanning/copying versus append-only settled blocks.
    let base = "A paragraph with **bold** and `code`.\n\n".repeat(4_000);
    let tokens = (0..2_000)
        .map(|index| format!("Streamed paragraph {index}.\n\n"))
        .collect::<Vec<_>>();

    let mut old_text = base.clone();
    let (_, tail) = split_streaming_markdown(&old_text);
    let mut old_view = (
        SharedString::from(&old_text[..old_text.len() - tail.len()]),
        SharedString::from(tail),
    );
    let start = Instant::now();
    for token in &tokens {
        old_text.push_str(token);
        let (_, tail) = split_streaming_markdown(&old_text);
        let split = old_text.len() - tail.len();
        if old_view.0.len() != split {
            old_view = (
                SharedString::from(&old_text[..split]),
                SharedString::from(tail),
            );
        } else {
            old_view.1 = SharedString::from(tail);
        }
        black_box(&old_view);
    }
    let old_split = start.elapsed();

    let mut new_message = ChatMessage::new(ChatRole::Assistant, String::new());
    new_message.streaming = true;
    for paragraph in base.split_inclusive("\n\n") {
        new_message.text.push_str(paragraph);
        refresh_stream_view(&mut new_message, true);
    }
    let start = Instant::now();
    for token in &tokens {
        new_message.text.push_str(token);
        refresh_stream_view(&mut new_message, token.contains('\n'));
        if let Some(view) = &new_message.stream {
            black_box((view.settled.clone(), view.tail.clone()));
        }
    }
    let new_split = start.elapsed();
    assert_eq!(new_message.text, old_text);
    assert_eq!(new_message.stream.as_ref().unwrap().tail, old_view.1,);

    // 2. Tool header: three JSON parses per frame before, a cached struct
    // borrow after.
    let big_input = format!(
        r#"{{"command":"echo hi","content":"{}"}}"#,
        "x".repeat(64 * 1024)
    );
    let start = Instant::now();
    for _ in 0..10_000 {
        black_box(tool_view(
            "write",
            &big_input,
            serde_json::from_str(&big_input).ok().as_ref(),
        ));
        black_box(shell_command(
            &big_input,
            serde_json::from_str(&big_input).ok().as_ref(),
        ));
        black_box(tool_content_body(
            "write",
            serde_json::from_str(&big_input).ok().as_ref(),
        ));
    }
    let old_tool = start.elapsed();

    let cached = ToolDisplay::new("write", &big_input);
    let start = Instant::now();
    for _ in 0..10_000 {
        black_box(&cached.view);
        black_box(cached.shell_command.as_ref());
        black_box(cached.content_body.as_ref());
    }
    let new_tool = start.elapsed();

    // 3. Transcript history: serialize + write + flush on the UI thread
    // before, serialize + enqueue after.
    let path = std::env::temp_dir().join(format!(
        "shob-stream-bench-{}-{}.jsonl",
        std::process::id(),
        paths::uid("b")
    ));
    let entry = conversations::Entry::Event {
        event: AgentEvent::Text("x".repeat(120)),
    };
    let encoded = serde_json::to_vec(&entry).unwrap();
    let start = Instant::now();
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        for _ in 0..2_000 {
            file.write_all(&encoded).unwrap();
            file.write_all(b"\n").unwrap();
            file.flush().unwrap();
        }
    }
    let old_history = start.elapsed();
    let _ = std::fs::remove_file(&path);

    let mut log = conversations::Log::at_path(path.clone(), u64::MAX);
    let conversations::Entry::Event { event } = &entry else {
        unreachable!()
    };
    let start = Instant::now();
    for _ in 0..2_000 {
        log.append_event(event);
    }
    let new_history_enqueue = start.elapsed();
    log.flush();
    let _ = std::fs::remove_file(&path);

    // 4. Burst coalescing.
    let burst = (0..10_000)
        .map(|index| AgentEvent::Text(format!("t{index} ")))
        .collect::<Vec<_>>();
    let burst_len = burst.len();
    let start = Instant::now();
    let coalesced = agent_bridge::coalesce_events(burst);
    let coalesce_time = start.elapsed();

    eprintln!("stream microbenchmark (2,000 paragraph deltas, ~144 KiB initial document)");
    eprintln!("  split + settled/tail copy: old={old_split:?} new={new_split:?}");
    eprintln!("  tool header (10,000 renders): old={old_tool:?} new={new_tool:?}");
    eprintln!(
        "  history foreground (2,000 appends): old={old_history:?} new={new_history_enqueue:?}"
    );
    eprintln!(
        "  coalesce 10,000 deltas: {burst_len} -> {} events in {coalesce_time:?}",
        coalesced.len()
    );
}

struct EnvVarGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, val: &std::ffi::OsStr) -> Self {
        let prev = std::env::var_os(key);
        // SAFETY: serialized in tests using this guard or unique temp dirs.
        unsafe { std::env::set_var(key, val) };
        Self { key, prev }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(prev) = &self.prev {
            // SAFETY: paired with EnvVarGuard::set.
            unsafe { std::env::set_var(self.key, prev) };
        } else {
            // SAFETY: paired with EnvVarGuard::set.
            unsafe { std::env::remove_var(self.key) };
        }
    }
}

fn test_git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Shob Test")
        .env("GIT_AUTHOR_EMAIL", "test@shob.local")
        .env("GIT_COMMITTER_NAME", "Shob Test")
        .env("GIT_COMMITTER_EMAIL", "test@shob.local")
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {:?} failed", args);
}

fn init_test_git_repo(dir: &Path, branch: &str) {
    std::fs::create_dir_all(dir).unwrap();
    test_git(dir, &["init", "-b", branch]);
    std::fs::write(dir.join("README.md"), "shob test\n").unwrap();
    test_git(dir, &["add", "README.md"]);
    test_git(dir, &["commit", "-m", "init"]);
}

#[test]
fn workspace_switcher_renders_context_strip_and_dropdown_only_in_one_mode() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    init_test_git_repo(&repo, "main");

    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);

    for agent_layout in [false, true] {
        cx.update_window_entity(&state, |s, _, cx| {
            s.projects[0].path = repo.clone();
            s.projects[0].name = SharedString::from("repo");
            s.settings.agent_layout = agent_layout;
            if let Some(pane) = s.pane_mut(pane_id) {
                pane.pid = None;
                pane.cwd = Some(repo.clone());
                pane.chat.active = agent_layout;
            }
            s.schedule_git_refresh(repo.clone(), cx);
            cx.notify();
        });
        cx.run_until_parked();
        cx.draw_frame().unwrap();
        if !agent_layout {
            assert!(cx.debug_bounds("composer-context-strip").is_none());
            continue;
        }

        assert!(cx.debug_bounds("composer-context-strip").is_some());
        let ws_btn = cx.debug_bounds("agent-workspace-selector").unwrap();
        assert!(cx.debug_bounds("agent-branch-selector").is_some());
        assert!(cx.debug_bounds("agent-project-selector").is_some());

        cx.simulate_click(ws_btn.center(), Modifiers::none());
        cx.draw_frame().unwrap();
        let menu = cx.debug_bounds("agent-workspace-menu").unwrap();
        assert!(menu.bottom() <= ws_btn.top() + px(2.0));
        assert!(cx.debug_bounds("workspace-option-local").is_some());
        assert!(cx.debug_bounds("workspace-option-new-worktree").is_some());

        // Close menu before next layout iteration.
        cx.simulate_click(ws_btn.center(), Modifiers::none());
        cx.draw_frame().unwrap();
        assert!(cx.debug_bounds("agent-workspace-menu").is_none());
    }

    cx.update_window_entity(&state, |s, _, cx| {
        let pane = s.pane_mut(pane_id).unwrap();
        pane.chat.messages.push(Arc::new(ChatMessage::new(
            ChatRole::User,
            "First prompt".into(),
        )));
        cx.notify();
    });
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("composer-context-strip").is_none());
    assert!(cx.debug_bounds("agent-composer-input").is_some());
}

#[test]
fn workspace_switcher_creates_switches_worktree_and_preserves_chat() {
    let temp = tempfile::tempdir().unwrap();
    let data_dir = temp.path().join("shob-data");
    let repo = temp.path().join("demo-repo");
    init_test_git_repo(&repo, "main");
    let _data_guard = EnvVarGuard::set("SHOB_DATA_DIR", data_dir.as_os_str());
    let _restore_guard = EnvVarGuard::set("SHOB_DISABLE_RESTORE", std::ffi::OsStr::new("1"));

    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let pane_id = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);

    cx.update_window_entity(&state, |s, window, cx| {
        s.projects[0].path = repo.clone();
        s.projects[0].name = SharedString::from("demo-repo");
        s.settings.agent_layout = true;
        s.set_chat_mode(pane_id, true, window, cx);
        if let Some(pane) = s.pane_mut(pane_id) {
            pane.pid = None;
            pane.cwd = Some(repo.clone());
            pane.chat.agent_draft = "draft text".into();
        }
        s.schedule_git_refresh(repo.clone(), cx);
        cx.notify();
    });
    cx.run_until_parked();
    cx.draw_frame().unwrap();

    let ws_btn = cx.debug_bounds("agent-workspace-selector").unwrap();
    cx.simulate_click(ws_btn.center(), Modifiers::none());
    cx.draw_frame().unwrap();

    let new_wt_btn = cx.debug_bounds("workspace-option-new-worktree").unwrap();
    cx.simulate_click(new_wt_btn.center(), Modifiers::none());
    cx.run_until_parked();
    cx.draw_frame().unwrap();

    let wt_path = state.read_with(&cx, |s, _| {
        let pane = s.pane_ref(pane_id).unwrap();
        assert_eq!(pane.chat.agent_draft, "draft text");
        let override_path = pane
            .chat
            .workspace_override
            .clone()
            .expect("worktree override");
        assert!(override_path.starts_with(data_dir.join("worktrees")));
        assert_eq!(s.agent_working_directory(pane), override_path);
        assert!(
            AppState::detect_git_branch(&override_path)
                .as_deref()
                .is_some_and(|b| b.starts_with("shob/"))
        );
        override_path
    });

    // Switch back to Current checkout and verify existing worktree stays listed.
    let ws_btn = cx.debug_bounds("agent-workspace-selector").unwrap();
    cx.simulate_click(ws_btn.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    assert!(cx.debug_bounds("workspace-option-worktree-0").is_some());
    let local_btn = cx.debug_bounds("workspace-option-local").unwrap();
    cx.simulate_click(local_btn.center(), Modifiers::none());
    cx.run_until_parked();
    cx.draw_frame().unwrap();

    state.read_with(&cx, |s, _| {
        let pane = s.pane_ref(pane_id).unwrap();
        assert_eq!(pane.chat.workspace_override, None);
        assert_eq!(s.agent_working_directory(pane), repo);
        assert_eq!(pane.chat.agent_draft, "draft text");
    });

    // Switch back to the existing worktree from the dropdown.
    let ws_btn = cx.debug_bounds("agent-workspace-selector").unwrap();
    cx.simulate_click(ws_btn.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    let existing_wt = cx.debug_bounds("workspace-option-worktree-0").unwrap();
    cx.simulate_click(existing_wt.center(), Modifiers::none());
    cx.run_until_parked();
    cx.draw_frame().unwrap();

    state.read_with(&cx, |s, _| {
        let pane = s.pane_ref(pane_id).unwrap();
        assert_eq!(pane.chat.workspace_override.as_ref(), Some(&wt_path));
    });
}

#[test]
fn project_switcher_switches_projects_and_preserves_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let repo_a = temp.path().join("alpha");
    let repo_b = temp.path().join("beta");
    init_test_git_repo(&repo_a, "main");
    init_test_git_repo(&repo_b, "develop");

    let mut app = TestAppContext::new();
    let (mut cx, state) = open_app(&mut app);
    let pane_a = state.read_with(&cx, |s, _| s.projects[0].sessions[0].panes[0].id);

    cx.update_window_entity(&state, |s, window, cx| {
        s.projects[0].path = repo_a.clone();
        s.projects[0].name = SharedString::from("alpha");
        s.settings.agent_layout = true;
        s.focus_one_chat(pane_a, window, cx);
        let pane_a = s.focused_pane_id.unwrap();
        if let Some(pane) = s.pane_mut(pane_a) {
            pane.pid = None;
            pane.cwd = Some(repo_a.clone());
            pane.chat.messages.push(Arc::new(ChatMessage::new(
                ChatRole::User,
                "alpha prompt".into(),
            )));
        }
        s.open_project_path(repo_b.clone(), window, cx);
        let pane_b = s.focused_pane_id.unwrap();
        if let Some(pane) = s.pane_mut(pane_b) {
            pane.pid = None;
            pane.cwd = Some(repo_b.clone());
            pane.chat.agent_draft = "beta draft".into();
        }
        s.schedule_git_refresh(repo_b.clone(), cx);
        cx.notify();
    });
    cx.run_until_parked();
    cx.draw_frame().unwrap();

    let proj_btn = cx.debug_bounds("agent-project-selector").unwrap();
    cx.simulate_click(proj_btn.center(), Modifiers::none());
    cx.draw_frame().unwrap();
    let menu = cx.debug_bounds("agent-project-menu").unwrap();
    let open_folder = cx.debug_bounds("project-option-open-folder").unwrap();
    assert!(menu.contains(&open_folder.origin));
    assert!(menu.contains(&open_folder.bottom_right()));
    let opt_alpha = cx.debug_bounds("project-option-0").unwrap();
    cx.simulate_click(opt_alpha.center(), Modifiers::none());
    cx.run_until_parked();
    cx.draw_frame().unwrap();

    state.read_with(&cx, |s, _| {
        assert_eq!(s.active_project_idx, 0);
        let pane_a = s.projects[0].sessions[0]
            .panes
            .iter()
            .find(|pane| pane.one_mode)
            .unwrap()
            .id;
        assert_eq!(s.focused_pane_id, Some(pane_a));
        let pane = s.pane_ref(pane_a).unwrap();
        assert_eq!(s.agent_working_directory(pane), repo_a);
        assert_eq!(pane.chat.messages[0].text.as_str(), "alpha prompt");
        let pane_b = s.projects[1].sessions[0]
            .panes
            .iter()
            .find(|pane| pane.one_mode)
            .unwrap();
        assert_eq!(pane_b.chat.agent_draft, "beta draft");
        assert_eq!(s.agent_working_directory(pane_b), repo_b);
    });
}
