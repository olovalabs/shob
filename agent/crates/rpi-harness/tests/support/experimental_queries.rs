use super::*;

#[tokio::test]
async fn typed_errors_match_memory_without_consuming_history() {
    let (_d, _repo, s) = fixture().await;
    let memory = InMemorySessionStorage::new(
        s.metadata(),
        Arc::new(FixedClock),
        Arc::new(CounterIdGenerator::new()),
    );
    for backend in [&s as &dyn SessionStorage, &memory as &dyn SessionStorage] {
        backend
            .append_entry(entry("existing"), "main")
            .await
            .unwrap();
        assert_eq!(
            backend
                .append_entry(entry("missing-lane"), "absent")
                .await
                .unwrap_err()
                .code,
            SessionErrorCode::InvalidLane
        );
        let mut r = record("record-missing-lane", "abort_requested");
        if let LaneRecord::AbortRequested(r) = &mut r {
            r.base.lane = "absent".into();
        }
        assert_eq!(
            backend.append_record(r).await.unwrap_err().code,
            SessionErrorCode::InvalidLane
        );
        assert_eq!(
            backend
                .append_entry(entry("existing"), "main")
                .await
                .unwrap_err()
                .code,
            SessionErrorCode::AlreadyExists
        );
        assert_eq!(
            backend
                .append_record(record("existing", "abort_requested"))
                .await
                .unwrap_err()
                .code,
            SessionErrorCode::AlreadyExists
        );
        assert_eq!(
            backend.create_lane("main", None).await.unwrap_err().code,
            SessionErrorCode::AlreadyExists
        );
        assert_eq!(
            backend.move_lane("absent", None).await.unwrap_err().code,
            SessionErrorCode::InvalidLane
        );
        assert_eq!(
            backend
                .move_lane("main", Some("absent"))
                .await
                .unwrap_err()
                .code,
            SessionErrorCode::NotFound
        );
        assert_eq!(
            backend
                .create_lane("side", Some("absent"))
                .await
                .unwrap_err()
                .code,
            SessionErrorCode::NotFound
        );
        assert_eq!(
            backend
                .set_label("absent", Some("label"))
                .await
                .unwrap_err()
                .code,
            SessionErrorCode::NotFound
        );
        assert_eq!(backend.history_revision().await.unwrap(), 1);
    }
}

#[tokio::test]
async fn branch_bounds_cursors_facts_and_context_match_real_reducer() {
    let (_d, repo, s) = fixture().await;
    let memory = InMemorySessionStorage::new(
        s.metadata(),
        Arc::new(FixedClock),
        Arc::new(CounterIdGenerator::new()),
    );
    let kinds = vec![
        entry("user-0"),
        ProvisionedEntry {
            id: "model".into(),
            kind: ProvisionedKind::ModelChange {
                provider: "fixture".into(),
                model_id: "model".into(),
            },
        },
        ProvisionedEntry {
            id: "thinking".into(),
            kind: ProvisionedKind::ThinkingLevel {
                thinking_level: "high".into(),
            },
        },
        entry("user-1"),
        ProvisionedEntry {
            id: "tools".into(),
            kind: ProvisionedKind::ActiveTools {
                active_tool_names: vec!["fixture".into()],
            },
        },
        ProvisionedEntry {
            id: "custom".into(),
            kind: ProvisionedKind::Custom {
                custom_type: "fixture".into(),
                data: Some(json!({"x":42})),
            },
        },
        ProvisionedEntry {
            id: "compact".into(),
            kind: ProvisionedKind::Compaction {
                summary: "summary".into(),
                retained_tail: vec![],
                tokens_before: 100,
                details: None,
                usage: None,
            },
        },
        ProvisionedEntry {
            id: "summary".into(),
            kind: ProvisionedKind::BranchSummary {
                from_id: "user-0".into(),
                summary: "branch summary".into(),
                details: None,
                usage: None,
            },
        },
        entry("latest"),
    ];
    for backend in [&s as &dyn SessionStorage, &memory as &dyn SessionStorage] {
        for item in kinds.clone() {
            backend.append_entry(item, "main").await.unwrap();
        }
        backend.create_lane("side", Some("user-1")).await.unwrap();
        backend
            .append_entry(entry("side-entry"), "side")
            .await
            .unwrap();
        for n in 0..12 {
            backend.set_name(Some(&format!("name-{n}"))).await.unwrap();
            backend
                .set_label("user-0", Some(&format!("label-{n}")))
                .await
                .unwrap();
        }
        backend.set_name(None).await.unwrap();
        backend.set_label("user-0", None).await.unwrap();
    }
    let metadata = s.metadata();
    drop(s);
    let s = repo.open(&metadata).await.unwrap();
    for start in ["latest", "side-entry", "custom"] {
        for bounds in [
            BranchBounds::default(),
            BranchBounds {
                stop_at_id: Some("user-1".into()),
                ..Default::default()
            },
            BranchBounds {
                stop_at_type: Some("message"),
                ..Default::default()
            },
            BranchBounds {
                stop_at_id: Some("not-on-branch".into()),
                ..Default::default()
            },
        ] {
            for order in [
                None,
                Some(EntryOrder::OldestFirst),
                Some(EntryOrder::NewestFirst),
            ] {
                for limit in [None, Some(1), Some(4)] {
                    for cursor in [None, Some(EntryCursor { after_seq: 5 })] {
                        for entry_type in [None, Some("message"), Some("custom")] {
                            let q = EntryQuery {
                                order,
                                limit,
                                cursor,
                                entry_type,
                                ..Default::default()
                            };
                            assert_eq!(
                                s.find_entries_on_branch(&q, &bounds, start).await.unwrap(),
                                memory
                                    .find_entries_on_branch(&q, &bounds, start)
                                    .await
                                    .unwrap(),
                                "start={start}, bounds={bounds:?}, q={q:?}"
                            );
                        }
                    }
                }
            }
            let actual = s.context_on_branch(&bounds, start).await.unwrap();
            let expected = memory.context_on_branch(&bounds, start).await.unwrap();
            assert_eq!(actual.messages, expected.messages);
            assert_eq!(actual.model, expected.model);
            assert_eq!(actual.thinking_level, expected.thinking_level);
            assert_eq!(actual.active_tool_names, expected.active_tool_names);
        }
    }
    for q in [
        EntryQuery {
            custom_type: Some("fixture".into()),
            ..Default::default()
        },
        EntryQuery {
            custom_type: Some("absent".into()),
            ..Default::default()
        },
    ] {
        assert_eq!(
            s.find_entries(&q).await.unwrap(),
            memory.find_entries(&q).await.unwrap()
        );
    }
    for after_seq in [None, Some(4), Some(30), Some(u64::MAX)] {
        for limit in [None, Some(1), Some(10)] {
            let q = LogOptions { after_seq, limit };
            assert_eq!(
                s.get_log(&q).await.unwrap(),
                memory.get_log(&q).await.unwrap()
            );
        }
    }
    assert_eq!(
        s.get_stats().await.unwrap(),
        memory.get_stats().await.unwrap()
    );
    assert_eq!(s.get_name().await.unwrap(), None);
    assert_eq!(s.get_label("user-0").await.unwrap(), None);
}

#[tokio::test]
async fn tool_outbox_record_filters_and_pending_queue_matrix_match_memory() {
    let (_d, _repo, s) = fixture().await;
    let memory = InMemorySessionStorage::new(
        s.metadata(),
        Arc::new(FixedClock),
        Arc::new(CounterIdGenerator::new()),
    );
    let mut assistant =
        rpi_ai::types::AssistantMessage::empty(rpi_ai::types::Api::Faux, "fixture", "model", 42);
    assistant.content.push(
        serde_json::from_value(
            json!({"type":"toolCall","id":"call","name":"fixture","arguments":{}}),
        )
        .unwrap(),
    );
    let outbox = rpi_agent::message::AgentMessage::Custom(rpi_agent::message::CustomMessage::new(
        "host",
        vec![],
        json!({"__rpiOutbox":{"key":"outbox-key"}}),
        42,
    ));
    let tool_result:rpi_agent::message::AgentMessage=serde_json::from_value(json!({"kind":"tool_result","role":"toolResult","toolCallId":"call","toolName":"fixture","content":[],"isError":false,"timestamp":42})).unwrap();
    let mut values = vec![
        json!({"type":"step_attempt","id":"step","runId":"run","step":"assistant","attempt":1,"resultEntryId":"assistant"}),
        json!({"type":"tool_started","id":"tool","runId":"run","assistantEntryId":"assistant","toolIndex":0,"toolCallId":"call","toolName":"fixture","effectiveArgs":{"x":42},"resultEntryId":"result","replay":"never"}),
        json!({"type":"write_deferred","id":"deferred","runId":"run","target":{"id":"later"}}),
        json!({"type":"assistant_frame","id":"legacy-frame","runId":"run","streamIndex":0,"op":"clear_run"}),
        json!({"type":"retry_pending","id":"retry","runId":"run","attempt":2,"resultEntryId":"retry-result"}),
        json!({"type":"usage","id":"usage","cause":"assistant","runId":"run","usage":{"input":10,"output":20,"cacheRead":3,"cacheWrite":2,"totalTokens":35,"cost":{"input":0.125,"output":0.25,"cacheRead":0.0,"cacheWrite":0.0,"total":0.375}}}),
        json!({"type":"abort_requested","id":"abort","runId":"run"}),
        json!({"type":"operation_finished","id":"finish-unknown","runId":"unknown","outcome":"completed"}),
    ];
    for value in &mut values {
        value["lane"] = json!("main");
        value["timestamp"] = json!(0);
        value["seq"] = json!(0);
    }
    for backend in [&s as &dyn SessionStorage, &memory as &dyn SessionStorage] {
        backend.append_entry(entry("user"), "main").await.unwrap();
        for (id, message) in [
            ("assistant", assistant.clone().into()),
            ("outbox", outbox.clone()),
            ("result", tool_result.clone()),
        ] {
            backend
                .append_entry(
                    ProvisionedEntry {
                        id: id.into(),
                        kind: ProvisionedKind::Message {
                            message,
                            terminate: None,
                        },
                    },
                    "main",
                )
                .await
                .unwrap();
        }
        backend.append_record(start("run")).await.unwrap();
        backend
            .append_records(
                values
                    .iter()
                    .map(|v| serde_json::from_value(v.clone()).unwrap())
                    .collect(),
            )
            .await
            .unwrap();
        backend
            .append_records(vec![
                queue("q-steer", "pending", "steer"),
                queue("q-follow", "pending", "followUp"),
                queue("q-next", "next", "nextRun"),
                queue("q-context", "context", "context"),
                queue("q-committed", "user", "steer"),
            ])
            .await
            .unwrap();
        backend
            .append_record(record("cancel", "queue_cancelled"))
            .await
            .unwrap();
    }
    for q in [
        EntryQuery {
            tool_call_id: Some("call".into()),
            ..Default::default()
        },
        EntryQuery {
            outbox_key: Some("outbox-key".into()),
            ..Default::default()
        },
        EntryQuery {
            message_boundary_only: true,
            ..Default::default()
        },
        EntryQuery {
            tool_call_id: Some("absent".into()),
            ..Default::default()
        },
    ] {
        assert_eq!(
            s.find_entries(&q).await.unwrap(),
            memory.find_entries(&q).await.unwrap()
        );
    }
    for record_type in [
        None,
        Some("operation_started"),
        Some("operation_finished"),
        Some("queue_enqueued"),
        Some("tool_started"),
        Some("assistant_frame"),
        Some("absent"),
    ] {
        for order in [None, Some(EntryOrder::OldestFirst)] {
            for pending_queue_only in [false, true] {
                for limit in [None, Some(1), Some(3)] {
                    let q = RecordQuery {
                        record_type,
                        order,
                        pending_queue_only,
                        limit,
                        ..Default::default()
                    };
                    assert_eq!(
                        s.find_records(&q).await.unwrap(),
                        memory.find_records(&q).await.unwrap(),
                        "q={q:?}"
                    );
                }
            }
        }
    }
    for q in [
        RecordQuery {
            tool_call_id: Some("call".into()),
            ..Default::default()
        },
        RecordQuery {
            run_id: Some("run".into()),
            after_seq: Some(5),
            ..Default::default()
        },
        RecordQuery {
            operation_kind: Some("run"),
            ..Default::default()
        },
        RecordQuery {
            queue_kind: Some(QueueKind::Context),
            ..Default::default()
        },
        RecordQuery {
            target_entry_id: Some("pending".into()),
            ..Default::default()
        },
    ] {
        assert_eq!(
            s.find_records(&q).await.unwrap(),
            memory.find_records(&q).await.unwrap()
        );
    }
    for after_seq in [None, Some(4), Some(15)] {
        for before_seq in [None, Some(15), Some(4)] {
            for from_end in [false, true] {
                let q = RecordPageQuery {
                    lane: "main".into(),
                    after_seq,
                    before_seq,
                    from_end,
                    limit: 3,
                };
                let a = s.record_page(&q).await.unwrap();
                let b = memory.record_page(&q).await.unwrap();
                assert_eq!(a.records, b.records);
                assert_eq!(a.total, b.total);
            }
        }
    }
    assert_eq!(
        s.get_stats().await.unwrap(),
        memory.get_stats().await.unwrap()
    );
}
