use rpi_harness::session::types::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn imported_inspection_does_not_pair_a_result_across_a_later_turn_boundary() {
    for user_boundary in [false, true] {
        let h = harness(None).await;
        let mut assistant = faux_assistant_message("", StopReason::ToolUse);
        assistant.content.push(Content::tool_call(
            "imported-id",
            "read",
            serde_json::json!({}),
        ));
        h.append_message(AgentMessage::Assistant(Box::new(assistant)))
            .await
            .unwrap();
        if user_boundary {
            h.append_message(rpi_ai::UserMessage::new("a later user turn", 0).into())
                .await
                .unwrap();
        } else {
            h.append_message(AgentMessage::Assistant(Box::new(faux_assistant_message(
                "a later assistant turn",
                StopReason::Stop,
            ))))
            .await
            .unwrap();
        }
        h.append_message(AgentMessage::ToolResult(Box::new(ToolResultMessage {
            role: ToolResultRole,
            tool_call_id: "imported-id".into(),
            tool_name: "read".into(),
            content: vec![],
            details: None,
            usage: None,
            added_tool_names: vec![],
            is_error: false,
            timestamp: 0,
        })))
        .await
        .unwrap();
        assert!(h
            .inspect_execution("imported-id")
            .await
            .unwrap()
            .unwrap()
            .result_entry_id
            .is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inspection_uses_checkpoint_result_identity_and_the_current_branch() {
    let h = harness(None).await;
    let mut assistant = faux_assistant_message("", StopReason::ToolUse);
    assistant.content.push(Content::tool_call(
        "checkpoint-call",
        "read",
        serde_json::json!({}),
    ));
    let call = h
        .append_message(AgentMessage::Assistant(Box::new(assistant)))
        .await
        .unwrap();
    h.session()
        .append_record(LaneRecord::ToolStarted(ToolStartedRecord {
            base: RecordBase {
                id: "checkpoint".into(),
                lane: "main".into(),
                seq: 0,
                timestamp: 0,
            },
            run_id: "checkpoint-run".into(),
            assistant_entry_id: call.clone(),
            tool_index: 0,
            tool_call_id: "checkpoint-call".into(),
            tool_name: "read".into(),
            effective_args: serde_json::json!({}),
            result_entry_id: "reserved-result".into(),
            replay: ToolReplay::Safe,
        }))
        .await
        .unwrap();
    let result = |name: &str| {
        AgentMessage::ToolResult(Box::new(ToolResultMessage {
            role: ToolResultRole,
            tool_call_id: "checkpoint-call".into(),
            tool_name: name.into(),
            content: vec![],
            details: None,
            usage: None,
            added_tool_names: vec![],
            is_error: false,
            timestamp: 0,
        }))
    };
    h.append_message(result("read")).await.unwrap();
    assert!(h
        .inspect_execution("checkpoint-call")
        .await
        .unwrap()
        .unwrap()
        .result_entry_id
        .is_none());
    h.session()
        .append_entry(
            ProvisionedEntry {
                id: "reserved-result".into(),
                kind: ProvisionedKind::Message {
                    message: result("read"),
                    terminate: None,
                },
            },
            "main",
        )
        .await
        .unwrap();
    let info = h
        .inspect_execution("checkpoint-call")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.entry_id.as_deref(), Some(call.as_str()));
    assert_eq!(info.result_entry_id.as_deref(), Some("reserved-result"));
    h.session().create_lane("empty", None).await.unwrap();
    assert!(h
        .lane("empty")
        .inspect_execution("checkpoint-call")
        .await
        .unwrap()
        .is_none());
    h.navigate_tree(Some(&call), false, None, None)
        .await
        .unwrap();
    assert!(h
        .inspect_execution("checkpoint-call")
        .await
        .unwrap()
        .unwrap()
        .result_entry_id
        .is_none());
}

async fn entered(gate: &Gate) {
    tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_batch_failure_preserves_all_items_and_can_retry() {
    let (_dir, path, gate, fs, h) = disk().await;
    h.steer(rpi_ai::UserMessage::new("steer draft", 0).into())
        .await
        .unwrap();
    h.follow_up(rpi_ai::UserMessage::new("follow draft", 0).into())
        .await
        .unwrap();
    let before = h.queued_messages().await.unwrap();
    gate.tear_cancel.store(true, Ordering::SeqCst);
    assert!(h.clear_queue().await.is_err());
    assert_eq!(h.queued_messages().await.unwrap(), before);
    let restored = h.clear_queue().await.unwrap();
    assert_eq!(restored, before);
    assert!(h.queued_messages().await.unwrap().is_empty());
    drop(h);
    assert!(reopen(&path, fs)
        .await
        .queued_messages()
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_enqueue_caller_does_not_abandon_memory_publication() {
    let (_dir, path, gate, fs, h) = disk().await;
    gate.block_kind.store(1, Ordering::SeqCst);
    let caller = {
        let h = h.clone();
        tokio::spawn(async move {
            h.steer(rpi_ai::UserMessage::new("accepted while caller left", 0).into())
                .await
        })
    };
    entered(&gate).await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    gate.release.notify_one();
    let queued = h.queued_messages().await.unwrap();
    assert_eq!(queued.steering.len(), 1);
    assert_eq!(queued.steering[0].text, "accepted while caller left");
    assert!(queued.steering[0].images.is_empty());
    drop(h);
    assert_eq!(
        reopen(&path, fs).await.queued_messages().await.unwrap(),
        queued
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_cancel_or_clear_caller_cannot_leave_a_ghost_queue() {
    for clear in [false, true] {
        let (_dir, path, gate, fs, h) = disk().await;
        let q = h
            .steer(rpi_ai::UserMessage::new("cancelled", 0).into())
            .await
            .unwrap();
        gate.block_kind.store(2, Ordering::SeqCst);
        let caller = {
            let h = h.clone();
            tokio::spawn(async move {
                if clear {
                    h.clear_queue().await.map(|_| ())
                } else {
                    h.cancel_queued(&q.entry_id).await.map(|_| ())
                }
            })
        };
        entered(&gate).await;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        gate.release.notify_one();
        assert!(h.queued_messages().await.unwrap().is_empty());
        drop(h);
        assert!(reopen(&path, fs)
            .await
            .queued_messages()
            .await
            .unwrap()
            .is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_prompt_caller_commits_reserved_input_only_once() {
    let (_dir, _path, gate, _fs, h) = disk().await;
    let q = h
        .next_run(rpi_ai::UserMessage::new("first-reserved", 0).into())
        .await
        .unwrap();
    gate.block_kind.store(3, Ordering::SeqCst);
    let caller = {
        let h = h.clone();
        tokio::spawn(async move { h.prompt_text("abandoned", vec![]).await })
    };
    entered(&gate).await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    gate.release.notify_one();
    h.prompt_text("retry", vec![]).await.unwrap();
    let entries = h
        .session()
        .find_entries(&EntryQuery {
            entry_id: Some(q.entry_id),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_prompt_failure_releases_only_uncommitted_next_run_items() {
    let (_dir, _path, gate, _fs, h) = disk().await;
    let first = h
        .next_run(rpi_ai::UserMessage::new("first-reserved", 0).into())
        .await
        .unwrap();
    let second = h
        .next_run(rpi_ai::UserMessage::new("second-reserved", 0).into())
        .await
        .unwrap();
    gate.fail_second.store(true, Ordering::SeqCst);
    assert!(h.prompt_text("first attempt", vec![]).await.is_err());
    assert!(h.get_entry(&first.entry_id).await.unwrap().is_some());
    assert!(h.get_entry(&second.entry_id).await.unwrap().is_none());
    h.prompt_text("retry", vec![]).await.unwrap();
    for id in [first.entry_id, second.entry_id] {
        assert_eq!(
            h.session()
                .find_entries(&EntryQuery {
                    entry_id: Some(id),
                    ..Default::default()
                })
                .await
                .unwrap()
                .len(),
            1
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_cancel_and_clear_cannot_consume_another_lanes_input() {
    let h = harness(None).await;
    h.session().create_lane("other", None).await.unwrap();
    let other = h.lane("other");
    let q = other
        .steer(rpi_ai::UserMessage::new("other input", 0).into())
        .await
        .unwrap();
    assert_eq!(
        h.cancel_queued(&q.entry_id).await.unwrap().outcome,
        rpi_harness::agent_harness::CancelQueuedOutcome::AlreadyConsumed
    );
    assert!(h.clear_queue().await.unwrap().is_empty());
    let queued = other.queued_messages().await.unwrap();
    assert_eq!(queued.steering.len(), 1);
    assert_eq!(queued.steering[0].text, "other input");
    assert!(queued.steering[0].images.is_empty());
    assert_eq!(other.clear_queue().await.unwrap(), queued);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_result_is_stable_after_jsonl_restore_and_branch_navigation() {
    let (_dir, path, _gate, fs, h) = disk().await;
    let first = h.prompt_text("first", vec![]).await.unwrap();
    h.prompt_text("second", vec![]).await.unwrap();
    drop(h);
    let h = reopen(&path, fs).await;
    h.navigate_tree(None, false, None, None).await.unwrap();
    let read = h.get_result(&first.run_id).await.unwrap().unwrap();
    assert_eq!(completed(&first), completed(&read));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_end_observer_reads_acked_result_without_changing_its_snapshot() {
    let h = Arc::new(harness(None).await);
    let weak = Arc::downgrade(&h);
    let saved = Arc::new(std::sync::Mutex::new(None));
    let out = saved.clone();
    let _listener = h
        .events()
        .on::<rpi_harness::events::RunEndEvent, _>(move |event| {
            let h = weak.upgrade().unwrap();
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let result = h.get_result(&event.run_id).await.unwrap().unwrap();
                    let id = h
                        .append_message(rpi_harness::messages::create_custom_message(
                            "observer",
                            "post-settlement context".into(),
                            true,
                            None,
                            0,
                        ))
                        .await
                        .unwrap();
                    (result, id)
                })
            });
            assert_eq!(completed(&result.0).0, event.leaf_id);
            *out.lock().unwrap() = Some(result);
        });
    let result = h.prompt_text("finish", vec![]).await.unwrap();
    let (during, added) = saved.lock().unwrap().take().unwrap();
    let after = h.get_result(&result.run_id).await.unwrap().unwrap();
    assert_eq!(completed(&result), completed(&during));
    assert_eq!(completed(&result), completed(&after));
    assert_ne!(
        h.get_leaf_id().await.unwrap().as_deref(),
        Some(completed(&result).0)
    );
    assert!(h.get_entry(&added).await.unwrap().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refused_terminal_ack_does_not_publish_run_end_or_a_queryable_result() {
    let (_dir, _path, gate, _fs, h) = disk().await;
    let count = Arc::new(AtomicUsize::new(0));
    let out = count.clone();
    let _listener = h
        .events()
        .on::<rpi_harness::events::RunEndEvent, _>(move |_| {
            out.fetch_add(1, Ordering::SeqCst);
        });
    gate.fail_terminal.store(true, Ordering::SeqCst);
    assert!(h.prompt_text("finish", vec![]).await.is_err());
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let open = h
        .session()
        .find_open_operations("main", None)
        .await
        .unwrap();
    assert_eq!(open.len(), 1);
    assert!(h.get_result(&open[0].base.id).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn legacy_terminal_without_identity_is_readable_but_does_not_invent_a_result() {
    let h = AgentHarness::create(Default::default()).await.unwrap();
    let base = |id: &str| RecordBase {
        id: id.into(),
        lane: "main".into(),
        seq: 0,
        timestamp: 0,
    };
    h.session()
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base("legacy"),
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        }))
        .await
        .unwrap();
    let record: LaneRecord = serde_json::from_value(serde_json::json!({
        "type":"operation_finished", "id":"legacy-finish", "lane":"main", "seq":0, "timestamp":0,
        "runId":"legacy", "outcome":"completed"
    }))
    .unwrap();
    h.session().append_record(record).await.unwrap();
    assert!(h
        .get_result("legacy")
        .await
        .unwrap_err()
        .to_string()
        .contains("legacy"));
    assert!(h
        .session()
        .find_open_operations("main", None)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_inspection_does_not_reuse_a_prior_calls_result() {
    let h = harness(None).await;
    let assistant = || {
        let mut message = faux_assistant_message("", StopReason::ToolUse);
        message
            .content
            .push(Content::tool_call("same-id", "read", serde_json::json!({})));
        AgentMessage::Assistant(Box::new(message))
    };
    h.append_message(assistant()).await.unwrap();
    h.append_message(AgentMessage::ToolResult(Box::new(ToolResultMessage {
        role: ToolResultRole,
        tool_call_id: "same-id".into(),
        tool_name: "read".into(),
        content: vec![],
        details: None,
        usage: None,
        added_tool_names: vec![],
        is_error: true,
        timestamp: 0,
    })))
    .await
    .unwrap();
    assert!(h
        .inspect_execution("same-id")
        .await
        .unwrap()
        .unwrap()
        .result_entry_id
        .is_some());
    let latest = h.append_message(assistant()).await.unwrap();
    let info = h.inspect_execution("same-id").await.unwrap().unwrap();
    assert_eq!(info.entry_id.as_deref(), Some(latest.as_str()));
    assert!(info.result_entry_id.is_none());
}
