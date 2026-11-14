//! Final entry ACK refusal must not invalidate an already-bound native ID or
//! publish AgentEnd before that entry is queryable.
use super::*;
use futures::future::BoxFuture;
use rpi_agent::{AgentEmitter, AgentEvent};
use rpi_ai::providers::faux::FauxStep;
use rpi_harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
use rpi_harness::session::{DefaultIdGenerator, SystemClock};
use rpi_harness::types::{CompactionSettings, HarnessTool, RetryPolicy};
use rpi_tools::{FileSystem, OsExecutionEnv};

struct LiveEvents {
    events: Mutex<Vec<AgentEvent>>,
    storage: Arc<ProbedStorage>,
    end_entries: Arc<Mutex<Vec<MessageEntry>>>,
}

impl AgentEmitter for LiveEvents {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        let final_id = {
            let mut events = self.events.lock().unwrap();
            let final_id = matches!(&event, AgentEvent::AgentEnd { messages }
                if matches!(messages.last(), Some(AgentMessage::Assistant(a)) if a.stop_reason != StopReason::Error))
            .then(|| {
                events
                    .iter()
                    .rev()
                    .find_map(|event| match event {
                        AgentEvent::MessageEnd {
                            message: AgentMessage::Assistant(_),
                            message_id,
                        } => message_id.clone(),
                        _ => None,
                    })
                    .expect("AgentEnd must follow a native final assistant")
            });
            events.push(event);
            final_id
        };
        let storage = self.storage.clone();
        let end_entries = self.end_entries.clone();
        Box::pin(async move {
            if let Some(id) = final_id {
                // Query inside the AgentEnd callback, not after run_core's
                // fallback has had a chance to make an early event look safe.
                let Some(Entry::Message(entry)) = storage.get_entry(&id).await.unwrap() else {
                    panic!("AgentEnd exposed final native ID {id} before its entry ACK")
                };
                assert!(matches!(&entry.message, AgentMessage::Assistant(_)));
                assert_eq!(entry.base.id, id);
                assert_eq!(
                    assistant_entries(&storage)
                        .await
                        .iter()
                        .filter(|entry| entry.base.id == id)
                        .count(),
                    1,
                    "the final native ID must name exactly one durable entry"
                );
                end_entries.lock().unwrap().push(entry);
            }
        })
    }

    fn try_emit(&self, event: AgentEvent) {
        assert!(!matches!(&event, AgentEvent::AgentEnd { .. }));
        self.events.lock().unwrap().push(event);
    }
}

struct Fixture {
    harness: AgentHarness,
    storage: Arc<ProbedStorage>,
    provider: Arc<FauxProvider>,
    live: Arc<LiveEvents>,
    ended: Arc<AtomicUsize>,
    fs: Arc<dyn FileSystem>,
    path: String,
    _dir: tempfile::TempDir,
}

impl Fixture {
    async fn new(volatile: bool, refusals: usize, refuse_binding: bool) -> Self {
        // Exactly one response: persistence fallback must never call a model.
        Self::configured(
            volatile,
            refusals,
            refuse_binding,
            FauxScript::new().with_text("finished once"),
            false,
        )
        .await
    }

    async fn configured(
        volatile: bool,
        refusals: usize,
        refuse_binding: bool,
        script: FauxScript,
        compact: bool,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
        let path = dir
            .path()
            .join("final.jsonl")
            .to_string_lossy()
            .into_owned();
        let inner = JsonlSessionStorage::create(
            fs.clone(),
            &path,
            JsonlV4Header::new("final-fallback".into(), 1, "/".into(), None, None, None),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let storage = Arc::new(ProbedStorage::new(Arc::new(inner)));
        storage.pause.store(false, Ordering::SeqCst);
        *storage.fail_assistants.lock().unwrap() = refusals;
        storage.fail_binding.store(refuse_binding, Ordering::SeqCst);
        let session = Session::new(storage.clone(), None);
        let session = if volatile {
            session.with_volatile_progress()
        } else {
            session
        };
        let provider = FauxProvider::new(script);
        let mut model = provider.default_model().clone();
        let mut tools = Vec::new();
        if compact {
            model.context_window = 4096;
            let env = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
            tools.push(HarnessTool::new(rpi_tools::create_write_tool(
                &rpi_tools::ExecutionToolContext::new(env.clone(), Some(env)),
            )));
        }
        let harness = AgentHarness::create(AgentHarnessOptions {
            model,
            models: vec![provider.clone()],
            tools,
            active_tool_names: if compact {
                vec!["write".into()]
            } else {
                vec![]
            },
            retry: RetryPolicy {
                enabled: true,
                max_retries: 1,
                base_delay_ms: 0,
                max_agent_delay_ms: 0,
            },
            compaction: CompactionSettings {
                enabled: compact,
                reserve_tokens: 20,
                keep_recent_tokens: 1,
            },
            session,
            ..Default::default()
        })
        .await
        .unwrap();
        let live = Arc::new(LiveEvents {
            events: Mutex::new(Vec::new()),
            storage: storage.clone(),
            end_entries: Arc::new(Mutex::new(Vec::new())),
        });
        harness.set_agent_emitter(Some(live.clone())).await.unwrap();
        let ended = Arc::new(AtomicUsize::new(0));
        let counter = ended.clone();
        harness
            .events()
            .on_lifetime::<rpi_harness::events::RunEndEvent, _>(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            });
        Self {
            harness,
            storage,
            provider,
            live,
            ended,
            fs,
            path,
            _dir: dir,
        }
    }

    fn final_live_id(&self) -> String {
        let events = self.live.events.lock().unwrap();
        let ids = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::MessageStart {
                    message: AgentMessage::Assistant(_),
                    message_id,
                }
                | AgentEvent::MessageEnd {
                    message: AgentMessage::Assistant(_),
                    message_id,
                } => Some(
                    message_id
                        .clone()
                        .expect("every live assistant has a native ID"),
                ),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 2, "one final assistant Start and End");
        assert_eq!(ids[0], ids[1]);
        ids[0].clone()
    }

    fn assert_agent_end(&self, id: &str, count: usize) {
        assert_eq!(
            self.live
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| matches!(event, AgentEvent::AgentEnd { messages }
                    if matches!(messages.last(), Some(AgentMessage::Assistant(a)) if a.stop_reason != StopReason::Error)))
                .count(),
            count,
            "AgentEnd must be withheld on failure and emitted exactly once on ACK"
        );
        let entries = self.live.end_entries.lock().unwrap();
        assert_eq!(entries.len(), count);
        for entry in entries.iter() {
            assert_eq!(entry.base.id, id, "callback queried the original native ID");
        }
    }

    async fn assert_unfinished(&self) -> String {
        self.assert_agent_end("", 0);
        assert!(records(&self.storage, "operation_finished")
            .await
            .is_empty());
        assert_eq!(self.ended.load(Ordering::SeqCst), 0);
        assert!(assistant_entries(&self.storage).await.is_empty());
        let open = self
            .storage
            .find_open_operations("main", None)
            .await
            .unwrap();
        assert_eq!(open.len(), 1);
        let id = open[0].base.id.clone();
        assert!(self.harness.get_result(&id).await.unwrap().is_none());
        assert_valid(self.storage.as_ref()).await;
        id
    }
}

async fn assert_valid(storage: &dyn SessionStorage) {
    rpi_harness::session::validate_record_log(&rpi_harness::session::RecordLogSlice {
        lane: "main".into(),
        open_operations: storage.find_open_operations("main", None).await.unwrap(),
        records: storage.find_records(&RecordQuery::default()).await.unwrap(),
        entries: storage.find_entries(&EntryQuery::default()).await.unwrap(),
    })
    .unwrap();
}

async fn assert_terminal_identity(storage: &dyn SessionStorage, run_id: &str, id: &str) {
    let records = storage
        .find_records(&RecordQuery {
            record_type: Some("operation_finished"),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    let LaneRecord::OperationFinished(record) = &records[0] else {
        unreachable!()
    };
    assert_eq!(record.run_id, run_id);
    assert_eq!(record.outcome, OperationOutcome::Completed);
    let identity = record.result.as_ref().unwrap();
    assert_eq!(identity.leaf_id, id);
    assert_eq!(identity.final_entry_id.as_deref(), Some(id));
    assert_eq!(identity.assistant_entry_id.as_deref(), Some(id));
    assert!(storage
        .find_open_operations("main", None)
        .await
        .unwrap()
        .is_empty());
    assert_valid(storage).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_entry_without_refusal_emits_one_durable_agent_end() {
    for volatile in [false, true] {
        let fixture = Fixture::new(volatile, 0, false).await;
        let result = fixture.harness.prompt_text("answer", vec![]).await.unwrap();
        let id = fixture.final_live_id();
        fixture.assert_agent_end(&id, 1);
        assert_eq!(
            *fixture.storage.assistant_attempts.lock().unwrap(),
            vec![id.clone()],
            "the normal path must not re-commit or re-emit at run end"
        );
        assert_eq!(fixture.ended.load(Ordering::SeqCst), 1);
        assert_terminal_identity(fixture.storage.as_ref(), &result.run_id, &id).await;
        assert!(rpi_harness::frame_progress::salvage_run_frames(
            fixture.harness.session(),
            &result.run_id,
        )
        .await
        .unwrap()
        .is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_final_entry_refusal_retries_the_same_native_id() {
    for volatile in [false, true] {
        let fixture = Fixture::new(volatile, 1, false).await;
        let result = fixture.harness.prompt_text("answer", vec![]).await.unwrap();
        let id = fixture.final_live_id();
        fixture.assert_agent_end(&id, 1);
        let HarnessRunOutcome::Completed {
            final_entry_id,
            final_message,
            ..
        } = &result.outcome
        else {
            panic!("one refused ACK must reach the same-ID fallback: {result:?}")
        };
        assert_eq!(final_entry_id, &id);
        assert_eq!(final_message.stop_reason, StopReason::Stop);
        assert_eq!(
            *fixture.storage.assistant_attempts.lock().unwrap(),
            vec![id.clone(), id.clone()],
            "settle and fallback must try the same reservation"
        );
        assert_eq!(*fixture.storage.fail_assistants.lock().unwrap(), 0);
        let entries = assistant_entries(&fixture.storage).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].base.id, id);
        assert_eq!(
            entries[0].message,
            AgentMessage::Assistant(Box::new(final_message.clone()))
        );
        assert_eq!(fixture.ended.load(Ordering::SeqCst), 1);
        let queried = fixture
            .harness
            .get_result(&result.run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(queried.run_id, result.run_id);
        let HarnessRunOutcome::Completed {
            leaf_id: query_leaf,
            final_entry_id: query_id,
            final_message: query_message,
        } = queried.outcome
        else {
            panic!("the acknowledged result must remain completed")
        };
        assert_eq!(query_leaf, id);
        assert_eq!(query_id, id);
        assert_eq!(query_message, *final_message);
        assert_terminal_identity(fixture.storage.as_ref(), &result.run_id, &id).await;
        assert!(fixture
            .storage
            .read_progress(&result.run_id)
            .await
            .unwrap()
            .is_empty());

        let restored = JsonlSessionStorage::load(
            fixture.fs.clone(),
            &fixture.path,
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let Some(Entry::Message(entry)) = restored.get_entry(&id).await.unwrap() else {
            panic!("the final native identity must survive JSONL reload")
        };
        assert_eq!(entry.message, entries[0].message);
        assert_terminal_identity(&restored, &result.run_id, &id).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_final_entry_refusal_keeps_the_operation_open() {
    for volatile in [false, true] {
        let fixture = Fixture::new(volatile, 2, false).await;
        let error = fixture
            .harness
            .prompt_text("answer", vec![])
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("controlled recovery entry failure"));
        let id = fixture.final_live_id();
        assert_eq!(
            *fixture.storage.assistant_attempts.lock().unwrap(),
            vec![id.clone(), id],
            "both refused writes must keep the bound identity"
        );
        assert_eq!(*fixture.storage.fail_assistants.lock().unwrap(), 0);
        let run_id = fixture.assert_unfinished().await;
        let salvaged =
            rpi_harness::frame_progress::salvage_run_frames(fixture.harness.session(), &run_id)
                .await
                .unwrap();
        assert_eq!(
            salvaged.len(),
            1,
            "failed ACKs must retain durable/volatile frames"
        );
        assert_eq!(
            rpi_ai::types::Content::text_only(&salvaged[0].content, ""),
            "finished once"
        );
        if !volatile {
            assert!(
                !fixture
                    .storage
                    .read_progress(&run_id)
                    .await
                    .unwrap()
                    .is_empty(),
                "persistent refusal must not retire recoverable content"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_final_binding_still_fails_closed_before_live_identity() {
    // Volatile bindings use an infallible in-memory path, not a storage ACK.
    let fixture = Fixture::new(false, 0, true).await;
    let error = fixture
        .harness
        .prompt_text("answer", vec![])
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("controlled identity binding failure"));
    assert!(!fixture.storage.fail_binding.load(Ordering::SeqCst));
    assert!(fixture
        .storage
        .assistant_attempts
        .lock()
        .unwrap()
        .is_empty());
    assert!(
        !fixture
            .live
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(
                event,
                AgentEvent::MessageStart { .. }
                    | AgentEvent::MessageUpdate { .. }
                    | AgentEvent::MessageEnd { .. }
            )),
        "an unbound identity must not be published"
    );
    fixture.assert_unfinished().await;
}

fn ended_assistant_ids(fixture: &Fixture) -> Vec<String> {
    let events = fixture.live.events.lock().unwrap();
    let starts = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageStart {
                message: AgentMessage::Assistant(_),
                message_id,
            } => Some(message_id.clone().expect("native start ID")),
            _ => None,
        })
        .collect::<Vec<_>>();
    let ends = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageEnd {
                message: AgentMessage::Assistant(_),
                message_id,
            } => Some(message_id.clone().expect("native end ID")),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts, ends, "each generation keeps its native identity");
    assert_eq!(
        starts.len(),
        starts
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    );
    ends
}

async fn assert_combined_final_refusal(
    fixture: &Fixture,
    refusals: usize,
    result: rpi_harness::result::HarnessResult<rpi_harness::agent_harness::RunResult>,
    final_id: &str,
) {
    let attempts = fixture.storage.assistant_attempts.lock().unwrap().clone();
    assert_eq!(
        attempts.iter().filter(|id| id.as_str() == final_id).count(),
        2
    );
    assert_eq!(*fixture.storage.fail_assistants.lock().unwrap(), 0);
    assert_eq!(
        fixture.provider.state().call_count.load(Ordering::SeqCst),
        if attempts.len() == 2 { 2 } else { 3 },
        "persistence fallback must not consume another provider response"
    );
    if refusals == 1 {
        let result = result.unwrap();
        let HarnessRunOutcome::Completed { final_entry_id, .. } = &result.outcome else {
            panic!("fallback must complete: {result:?}")
        };
        assert_eq!(final_entry_id, final_id);
        fixture.assert_agent_end(final_id, 1);
        assert_eq!(fixture.ended.load(Ordering::SeqCst), 1);
        assert_terminal_identity(fixture.storage.as_ref(), &result.run_id, final_id).await;
        assert!(fixture
            .storage
            .read_progress(&result.run_id)
            .await
            .unwrap()
            .is_empty());
    } else {
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("controlled recovery entry failure"));
        fixture.assert_agent_end(final_id, 0);
        assert_eq!(fixture.ended.load(Ordering::SeqCst), 0);
        assert!(records(&fixture.storage, "operation_finished")
            .await
            .is_empty());
        assert!(fixture.storage.get_entry(final_id).await.unwrap().is_none());
        let open = fixture
            .storage
            .find_open_operations("main", None)
            .await
            .unwrap();
        assert_eq!(open.len(), 1);
        assert!(fixture
            .harness
            .get_result(&open[0].base.id)
            .await
            .unwrap()
            .is_none());
        let salvaged = rpi_harness::frame_progress::salvage_run_frames(
            fixture.harness.session(),
            &open[0].base.id,
        )
        .await
        .unwrap();
        assert_eq!(salvaged.len(), 1);
        assert_eq!(
            rpi_ai::types::Content::text_only(&salvaged[0].content, ""),
            "finished once"
        );
        assert_valid(fixture.storage.as_ref()).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_retry_then_final_ack_refusal_does_not_replay_the_failed_attempt_end() {
    for volatile in [false, true] {
        for refusals in [1, 2] {
            let mut failed = faux_assistant_message("discarded attempt", StopReason::Error);
            failed.error_message = Some("503 service unavailable".into());
            let script = FauxScript::new();
            script.set_responses(vec![
                FauxStep::message(failed),
                FauxStep::text("finished once"),
            ]);
            let fixture = Fixture::configured(volatile, refusals, false, script, false).await;
            let result = fixture.harness.prompt_text("answer", vec![]).await;
            let ids = ended_assistant_ids(&fixture);
            assert_eq!(ids.len(), 2);
            assert!(fixture.storage.get_entry(&ids[0]).await.unwrap().is_none());
            assert_eq!(
                *fixture.storage.assistant_attempts.lock().unwrap(),
                vec![ids[1].clone(); 2]
            );
            {
                let events = fixture.live.events.lock().unwrap();
                let retry_at = events
                    .iter()
                    .position(|event| {
                        matches!(event, AgentEvent::RetryScheduled { attempt: 1, .. })
                    })
                    .expect("real provider retry");
                // The existing contract exposes the failed attempt's own End.
                // It must not be replayed as the successful attempt's held End.
                assert_eq!(events.iter().filter(|event| matches!(event,
                    AgentEvent::AgentEnd { messages }
                    if matches!(messages.last(), Some(AgentMessage::Assistant(a)) if a.stop_reason == StopReason::Error))).count(), 1);
                assert!(!events[retry_at..].iter().any(|event| matches!(event,
                    AgentEvent::AgentEnd { messages }
                    if matches!(messages.last(), Some(AgentMessage::Assistant(a)) if a.stop_reason == StopReason::Error))));
            }
            assert_combined_final_refusal(&fixture, refusals, result, &ids[1]).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mid_run_compaction_then_final_ack_refusal_releases_only_the_durable_final_end() {
    for volatile in [false, true] {
        for refusals in [1, 2] {
            let script = FauxScript::new();
            let fixture = Fixture::configured(volatile, 0, false, script.clone(), true).await;
            let storage = fixture.storage.clone();
            script.set_responses(vec![
                FauxStep::tool_call(
                    "write",
                    serde_json::json!({
                        "path": "large.txt", "content": "compaction input ".repeat(5000),
                    }),
                ),
                FauxStep::factory(|_| {
                    faux_assistant_message("## Goal\nContinue.", StopReason::Stop)
                }),
                FauxStep::factory(move |ctx| {
                    assert!(
                        serde_json::to_string(&ctx.messages)
                            .unwrap()
                            .contains("## Goal"),
                        "the final generation must use the compacted context"
                    );
                    // Refuse only the final assistant, not the tool assistant
                    // that must already be durable for compaction to proceed.
                    *storage.fail_assistants.lock().unwrap() = refusals;
                    faux_assistant_message("finished once", StopReason::Stop)
                }),
            ]);
            let result = fixture.harness.prompt_text("answer", vec![]).await;
            let ids = ended_assistant_ids(&fixture);
            assert_eq!(ids.len(), 2, "tool assistant and final assistant");
            let entries = fixture
                .storage
                .find_entries(&EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })
                .await
                .unwrap();
            let tool_at = entries
                .iter()
                .position(|entry| entry.id() == ids[0])
                .unwrap();
            let compact_at = entries
                .iter()
                .position(|entry| matches!(entry, Entry::Compaction(_)))
                .expect("actual mid-run compaction must set a nonzero persistence cut");
            assert!(tool_at < compact_at, "not pre-run compaction");
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| matches!(entry, Entry::Compaction(_)))
                    .count(),
                1
            );
            assert_eq!(
                *fixture.storage.assistant_attempts.lock().unwrap(),
                vec![ids[0].clone(), ids[1].clone(), ids[1].clone()]
            );
            assert_combined_final_refusal(&fixture, refusals, result, &ids[1]).await;
        }
    }
}
