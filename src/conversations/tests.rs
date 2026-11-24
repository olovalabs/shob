use super::*;

#[test]
fn delete_pane_waits_for_queued_writes_and_preserves_other_panes() {
    let root = std::env::temp_dir().join(crate::paths::uid("conversation-delete-test"));
    let deleted = root.join("deleted");
    let retained = root.join("retained");
    let mut first = Log::at_path(deleted.join("one.jsonl"), u64::MAX);
    let mut second = Log::at_path(retained.join("two.jsonl"), u64::MAX);
    first.append_user("private");
    second.append_user("keep");
    delete_directory(deleted.clone()).unwrap();
    assert!(!deleted.exists());
    assert!(retained.join("two.jsonl").exists());
    delete_directory(deleted).unwrap();
    drop(first);
    drop(second);
    let _ = fs::remove_dir_all(root);
}

fn temp_log(name: &str, max_bytes: u64) -> Log {
    let path = std::env::temp_dir().join(format!(
        "shob-conversations-{name}-{}.jsonl",
        paths::uid("t")
    ));
    Log::at_path(path, max_bytes)
}

#[test]
fn entries_roundtrip_as_json() {
    let entries = vec![
        Entry::User {
            text: "hello".into(),
        },
        Entry::Event {
            event: AgentEvent::Text("hi".into()),
        },
        Entry::Event {
            event: AgentEvent::ToolStart {
                id: Some("1".into()),
                name: "read".into(),
                input: "{}".into(),
            },
        },
        Entry::Local {
            role: ChatRole::Status,
            text: "/fast on".into(),
        },
    ];
    for entry in entries {
        let text = serde_json::to_string(&entry).unwrap();
        let _: Entry = serde_json::from_str(&text).unwrap();
    }
}

#[test]
fn transcript_replays_user_and_agent_messages() {
    let mut log = temp_log("replay", MAX_FILE_BYTES);
    log.append_user("fix the bug");
    log.append_event(&AgentEvent::Text("looking".into()));
    log.append_event(&AgentEvent::Text(" now".into()));
    log.append_event(&AgentEvent::ToolStart {
        id: Some("t1".into()),
        name: "read".into(),
        input: "src/main.rs".into(),
    });
    log.append_event(&AgentEvent::ToolEnd {
        id: Some("t1".into()),
        name: "read".into(),
        output: "ok".into(),
        failed: false,
    });
    log.append_local(ChatRole::Status, "/fast on");
    log.flush();
    let messages = load(log.path());

    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].role, ChatRole::User);
    assert_eq!(messages[0].text, "fix the bug");
    // Consecutive text deltas coalesce into one assistant message.
    assert_eq!(messages[1].role, ChatRole::Assistant);
    assert_eq!(messages[1].text, "looking now");
    assert!(!messages[1].streaming);
    assert_eq!(messages[2].role, ChatRole::Tool);
    let tool = messages[2].tool.as_ref().unwrap();
    assert_eq!(tool.state, ToolState::Completed);
    assert_eq!(tool.output, "ok");
    assert_eq!(messages[3].role, ChatRole::Status);
    assert_eq!(messages[3].text, "/fast on");

    let path = log.path().to_path_buf();
    drop(log);
    let _ = fs::remove_file(path);
}

#[test]
fn usage_replays_multiple_finalized_messages() {
    let mut log = temp_log("usage-messages", MAX_FILE_BYTES);
    log.append_user("first");
    log.append_event(&AgentEvent::Text("answer".into()));
    log.append_event(&AgentEvent::Usage(embedded::AgentUsage {
        input: 100,
        output: 20,
        cache_read: 40,
        cache_write: 10,
        cost: Some(0.125),
    }));
    log.append_event(&AgentEvent::Done);
    log.append_user("second");
    log.append_event(&AgentEvent::Text("another answer".into()));
    log.append_event(&AgentEvent::Usage(embedded::AgentUsage {
        input: 200,
        output: 30,
        cache_read: 80,
        cache_write: 15,
        cost: Some(0.25),
    }));
    log.append_event(&AgentEvent::Done);
    log.flush();

    assert_eq!(
        load_usage(log.path()),
        Some(embedded::AgentUsage {
            input: 300,
            output: 50,
            cache_read: 120,
            cache_write: 25,
            cost: Some(0.375),
        })
    );
    let messages = load(log.path());
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1].text, "answer");
    assert_eq!(messages[3].text, "another answer");
    assert!(messages.iter().all(|message| !message.streaming));
    // Replaying is read-only and never adds the deltas a second time.
    assert_eq!(
        load_usage(log.path()),
        Some(embedded::AgentUsage {
            input: 300,
            output: 50,
            cache_read: 120,
            cache_write: 25,
            cost: Some(0.375),
        })
    );

    let path = log.path().to_path_buf();
    drop(log);
    fs::remove_file(path).unwrap();
}

#[test]
fn usage_replay_preserves_unknown_cost() {
    for costs in [[Some(0.25), None, Some(0.5)], [None, Some(0.25), Some(0.5)]] {
        let mut log = temp_log("usage-unknown-cost", MAX_FILE_BYTES);
        for cost in costs {
            log.append_event(&AgentEvent::Usage(embedded::AgentUsage {
                input: 10,
                output: 2,
                cache_read: 3,
                cache_write: 4,
                cost,
            }));
        }
        log.flush();
        assert_eq!(
            load_usage(log.path()),
            Some(embedded::AgentUsage {
                input: 30,
                output: 6,
                cache_read: 9,
                cache_write: 12,
                cost: None,
            })
        );
        let path = log.path().to_path_buf();
        drop(log);
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn usage_replay_skips_corrupt_lines_and_absent_usage() {
    let mut log = temp_log("usage-corrupt", MAX_FILE_BYTES);
    assert_eq!(load_usage(log.path()), None);
    log.append_user("hello");
    log.flush();
    assert_eq!(load_usage(log.path()), None);

    let usage = embedded::AgentUsage {
        input: 15,
        output: 5,
        cache_read: 2,
        cache_write: 1,
        cost: Some(0.0),
    };
    let entry = serde_json::to_string(&Entry::Event {
        event: AgentEvent::Usage(usage),
    })
    .unwrap();
    fs::write(
        log.path(),
        format!("not json\n\n{entry}\n{{\"t\":\"event\",\"event\":null}}\n{entry}\n{{"),
    )
    .unwrap();
    assert_eq!(
        load_usage(log.path()),
        Some(embedded::AgentUsage {
            input: 30,
            output: 10,
            cache_read: 4,
            cache_write: 2,
            cost: Some(0.0),
        })
    );
    assert!(load(log.path()).is_empty());

    let path = log.path().to_path_buf();
    drop(log);
    fs::remove_file(path).unwrap();
}

#[test]
fn usage_replay_uses_the_transcript_read_bound() {
    let log = temp_log("usage-bounded", MAX_FILE_BYTES);
    let usage = embedded::AgentUsage {
        input: 10,
        output: 2,
        cache_read: 3,
        cache_write: 4,
        cost: Some(0.125),
    };
    let entry = serde_json::to_string(&Entry::Event {
        event: AgentEvent::Usage(usage),
    })
    .unwrap();
    let mut file = File::create(log.path()).unwrap();
    writeln!(file, "{entry}").unwrap();
    // A sparse oversized line puts the first usage outside the read window.
    file.seek(SeekFrom::Start(MAX_FILE_BYTES + 1024)).unwrap();
    writeln!(file).unwrap();
    writeln!(file, "{entry}").unwrap();
    writeln!(
        file,
        "{}",
        serde_json::to_string(&Entry::User {
            text: "retained".into(),
        })
        .unwrap()
    )
    .unwrap();
    drop(file);

    assert_eq!(load_usage(log.path()), Some(usage));
    let messages = load(log.path());
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].text, "retained");
    let path = log.path().to_path_buf();
    drop(log);
    fs::remove_file(path).unwrap();
}

#[test]
fn queued_writes_keep_per_conversation_order() {
    let mut first = temp_log("order-first", MAX_FILE_BYTES);
    let mut second = temp_log("order-second", MAX_FILE_BYTES);
    first.append_event(&AgentEvent::Text("one".into()));
    second.append_event(&AgentEvent::Text("other".into()));
    first.append_event(&AgentEvent::Text(" two".into()));
    first.flush();

    let first_messages = load(first.path());
    assert_eq!(first_messages.len(), 1);
    assert_eq!(first_messages[0].text, "one two");
    let second_messages = load(second.path());
    assert_eq!(second_messages.len(), 1);
    assert_eq!(second_messages[0].text, "other");

    let first_path = first.path().to_path_buf();
    let second_path = second.path().to_path_buf();
    drop(first);
    drop(second);
    let _ = fs::remove_file(first_path);
    let _ = fs::remove_file(second_path);
}

#[test]
fn running_tools_are_not_left_spinning() {
    let mut log = temp_log("running", MAX_FILE_BYTES);
    log.append_event(&AgentEvent::ToolStart {
        id: Some("t1".into()),
        name: "read".into(),
        input: "".into(),
    });
    log.flush();
    let messages = load(log.path());
    assert_eq!(messages[0].tool.as_ref().unwrap().state, ToolState::Unknown);
    let path = log.path().to_path_buf();
    drop(log);
    let _ = fs::remove_file(path);
}

#[test]
fn large_logs_rotate_and_keep_complete_lines() {
    let mut log = temp_log("rotate", 300);
    for index in 0..20 {
        log.append_event(&AgentEvent::Text(format!("line {index}")));
    }
    log.flush();
    let rotated = log.path().with_extension("jsonl.1");
    assert!(rotated.exists(), "expected the first generation to rotate");
    let current = load(log.path());
    assert!(!current.is_empty());
    assert!(fs::metadata(log.path()).unwrap().len() <= 300);
    let path = log.path().to_path_buf();
    drop(log);
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(rotated);
}

#[test]
fn conversation_keys_are_filesystem_safe() {
    let model = AgentModel {
        provider: "google/antigravity".into(),
        id: "gemini:3.1".into(),
        backend: ModelBackend::Shob,
    };
    let key = conversation_key(Some(&model));
    assert_eq!(key, "shob_google_antigravity_gemini_3.1");
    assert_eq!(conversation_key(None), "default");
    let embedded = AgentModel {
        backend: ModelBackend::Embedded,
        ..model
    };
    assert_eq!(
        conversation_key(Some(&embedded)),
        "embedded_google_antigravity_gemini_3.1"
    );
    assert_eq!(
        legacy_conversation_key(Some(&embedded)),
        "embedded_google_antigravity_gemini_3.1"
    );
}

#[test]
fn capped_text_respects_utf8_boundaries() {
    let text = "α".repeat(100);
    let capped = capped(&text, 10);
    assert!(capped.starts_with("ααααα"));
    assert!(capped.ends_with("(truncated)"));
}

#[test]
fn projection_caps_tool_results_without_changing_live_payload() {
    let event = AgentEvent::ToolEnd {
        id: Some("call".into()),
        name: "bash".into(),
        output: "α".repeat(2 * 1024 * 1024),
        failed: true,
    };
    let projected = project_event(&event);
    assert!(
        matches!(&event, AgentEvent::ToolEnd { output, .. } if output.len() == 4 * 1024 * 1024)
    );
    assert!(
        matches!(&projected, AgentEvent::ToolEnd { id, name, output, failed }
            if id.as_deref() == Some("call") && name == "bash" && *failed &&
                output.len() < MAX_ENTRY_BYTES + 32 && output.ends_with("(truncated)"))
    );
    assert!(matches!(project_event(&AgentEvent::Done), AgentEvent::Done));
    assert!(
        matches!(project_event(&AgentEvent::Error("error".into())), AgentEvent::Error(text) if text == "error")
    );
}

#[test]
fn stalled_history_writer_reports_backpressure_without_waiting() {
    let (sender, receiver) = mpsc::sync_channel(1);
    let bytes = Arc::new(AtomicUsize::new(0));
    let mut log = Log {
        path: PathBuf::from("stalled-history"),
        max_bytes: MAX_FILE_BYTES,
        writer: Writer {
            sender,
            bytes: Arc::clone(&bytes),
        },
        error: Arc::new(Mutex::new(None)),
    };
    log.append_event(&AgentEvent::Text("accepted".into()));
    let charged = bytes.load(Ordering::Relaxed);
    assert!(charged > 0);
    let start = std::time::Instant::now();
    log.append_event(&AgentEvent::Done);
    assert!(start.elapsed() < std::time::Duration::from_millis(50));
    assert!(log.take_error().unwrap().contains("queue is full"));
    assert!(log.take_error().is_none());
    assert_eq!(bytes.load(Ordering::Relaxed), charged);
    drop(receiver.recv().unwrap());
    assert_eq!(bytes.load(Ordering::Relaxed), 0);
    drop(receiver);
    log.append_user("disconnected");
    assert!(log.take_error().unwrap().contains("writer stopped"));
    assert_eq!(bytes.load(Ordering::Relaxed), 0);
}

#[test]
fn history_byte_budget_and_disk_errors_are_observable() {
    let (sender, receiver) = mpsc::sync_channel(MAX_QUEUED_ENTRIES);
    let bytes = Arc::new(AtomicUsize::new(0));
    let mut log = Log {
        path: PathBuf::from("byte-budget"),
        max_bytes: MAX_FILE_BYTES,
        writer: Writer {
            sender,
            bytes: Arc::clone(&bytes),
        },
        error: Arc::new(Mutex::new(None)),
    };
    let event = AgentEvent::Text("x".repeat(MAX_ENTRY_BYTES));
    for _ in 0..MAX_QUEUED_ENTRIES {
        log.append_event(&event);
    }
    assert!(bytes.load(Ordering::Relaxed) <= MAX_QUEUED_BYTES);
    assert!(log.take_error().unwrap().contains("queue is full"));
    drop(receiver);
    assert_eq!(bytes.load(Ordering::Relaxed), 0);

    let file = std::env::temp_dir().join(paths::uid("history-error"));
    fs::write(&file, "not a directory").unwrap();
    let mut failed = Log::at_path(file.join("history.jsonl"), MAX_FILE_BYTES);
    failed.append_user("hello");
    failed.flush();
    assert!(failed.take_error().unwrap().contains("Failed to append"));
    drop(failed);
    fs::remove_file(file).unwrap();
}

#[test]
#[ignore = "manual transcript projection cost comparison"]
fn transcript_projection_benchmark() {
    let event = AgentEvent::ToolUpdate {
        id: Some("bench".into()),
        name: "bash".into(),
        output: "x".repeat(1024 * 1024),
    };
    let start = std::time::Instant::now();
    for _ in 0..1000 {
        std::hint::black_box(project_event(std::hint::black_box(&event)));
    }
    let capped = start.elapsed();
    let start = std::time::Instant::now();
    for _ in 0..1000 {
        std::hint::black_box(std::hint::black_box(&event).clone());
    }
    eprintln!(
        "transcript projection: capped={capped:?}, full_clone={:?}, retained_prefix_limit={MAX_ENTRY_BYTES}",
        start.elapsed()
    );
}
