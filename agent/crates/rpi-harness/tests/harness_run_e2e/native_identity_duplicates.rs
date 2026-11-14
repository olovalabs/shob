//! Duplicate provider IDs must not alias native execution/result reservations.
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicate_tool_ids_reject_entire_batch_before_start_and_preserve_reducer() {
    for mode in [
        HarnessToolExecution::Parallel,
        HarnessToolExecution::Sequential,
    ] {
        for stop in [StopReason::ToolUse, StopReason::Length] {
            for deferred_run in [false, true] {
                for names in [["", "read"], ["read", ""], ["read", "read"]] {
                    let (session, fs) = fixture(false).await;
                    let mut assistant = faux_assistant_message("", stop);
                    assistant.content = names
                        .into_iter()
                        .map(|name| {
                            rpi_ai::Content::tool_call(
                                "shared",
                                name,
                                serde_json::json!({"path":"a.txt"}),
                            )
                        })
                        .collect();
                    let script = FauxScript::new();
                    script.set_responses(if deferred_run {
                        vec![FauxStep::text("done")]
                    } else {
                        vec![FauxStep::message(assistant.clone()), FauxStep::text("done")]
                    });
                    let provider = FauxProvider::new(script);
                    let deferred = handle(&provider);
                    if deferred_run {
                        provider.push_deferred_poll(DeferredPoll::Message(assistant));
                    }
                    let harness = identity_harness(session.clone(), false, provider, mode).await;
                    let (collector, events) = CollectorEmitter::new();
                    harness
                        .set_agent_emitter(Some(Arc::new(collector)))
                        .await
                        .unwrap();
                    let result = if deferred_run {
                        park(&harness, &deferred).await;
                        tokio::time::timeout(Duration::from_secs(5), harness.resume(&deferred.id))
                            .await
                    } else {
                        tokio::time::timeout(
                            Duration::from_secs(5),
                            harness.prompt_text("reject ambiguous batch", vec![]),
                        )
                        .await
                    }
                    .expect("bounded rejection")
                    .expect("batch rejects without identity failure");
                    assert!(matches!(
                        result.outcome,
                        HarnessRunOutcome::Completed { .. }
                    ));
                    let events = events.lock().unwrap().clone();
                    assert_live_entry_ids(&harness, &events, false).await;
                    assert!(!events.iter().any(|event| matches!(
                        event,
                        AgentEvent::ToolExecutionStart { .. }
                            | AgentEvent::ToolExecutionUpdate { .. }
                    )));
                    let ids = events
                        .iter()
                        .filter_map(|event| match event {
                            AgentEvent::MessageEnd {
                                message: AgentMessage::ToolResult(result),
                                message_id,
                            } => {
                                assert_eq!(result.tool_call_id, "shared");
                                assert!(result.is_error);
                                Some(message_id.as_ref().unwrap())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(ids.len(), 2);
                    assert_ne!(
                        ids[0], ids[1],
                        "independent rejected messages own native IDs"
                    );
                    assert!(!session
                        .find_records(&RecordQuery::default())
                        .await
                        .unwrap()
                        .iter()
                        .any(|record| matches!(record, LaneRecord::ToolStarted(_))));
                    validate_native_session(&session).await;
                    drop(harness);
                    let session = reopen_session(session, fs, false).await;
                    validate_native_session(&session).await;
                    let reopened = reopen_harness(session.clone()).await;
                    assert_live_entry_ids(&reopened, &events, false).await;
                    validate_native_session(&session).await;
                }
            }
        }
    }
}
