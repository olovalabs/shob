//! Exercise the durable history and provider request, rather than just the
//! converter: context injected from inside a running tool must not split its
//! call/result batch, trigger an extra turn, or disappear on restart.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, Weak,
};

use futures::future::BoxFuture;
use rpi_agent::{AgentEmitter, AgentEvent, AgentMessage};
use rpi_ai::providers::faux::{
    faux_assistant_message, FauxBlock, FauxProvider, FauxScript, FauxStep,
};
use rpi_ai::types::{Content, Message, StopReason, UserContent};
use rpi_ai::Provider;
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome, SendMessageResult};
use rpi_harness::messages::create_custom_message;
use rpi_harness::session::memory::{InMemorySessionStorage, SystemClock};
use rpi_harness::session::types::{
    Entry, EntryOrder, EntryQuery, LaneRecord, RecordQuery, SessionMetadata,
};
use rpi_harness::session::{DefaultIdGenerator, Session};
use rpi_harness::types::{AgentHarnessOptions, HarnessTool, MessageDelivery, SendMessageOptions};
use rpi_tools::{create_write_tool, ExecutionToolContext, InMemoryExecutionEnv};

fn context_message() -> AgentMessage {
    create_custom_message(
        "goal",
        UserContent::Text("goal state".into()),
        true,
        None,
        0,
    )
}

struct AppendDuringEvent {
    harness: Mutex<Weak<AgentHarness>>,
    trigger: fn(&AgentEvent) -> bool,
    fired: AtomicBool,
    reserved: Arc<Mutex<Vec<String>>>,
    observed: Arc<Mutex<Vec<AgentEvent>>>,
    ready: Arc<tokio::sync::Notify>,
    proceed: Arc<tokio::sync::Notify>,
    pause: bool,
    cancel: bool,
    message: AgentMessage,
    send: Option<SendMessageOptions>,
    pause_after_delivery: bool,
    delivered: Arc<tokio::sync::Notify>,
}

impl AppendDuringEvent {
    fn new(trigger: fn(&AgentEvent) -> bool) -> Self {
        Self {
            harness: Default::default(),
            trigger,
            fired: AtomicBool::new(false),
            reserved: Default::default(),
            observed: Default::default(),
            ready: Default::default(),
            proceed: Default::default(),
            pause: false,
            cancel: false,
            message: context_message(),
            send: None,
            pause_after_delivery: false,
            delivered: Default::default(),
        }
    }
}

impl AgentEmitter for AppendDuringEvent {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        let harness = self.harness.lock().unwrap().upgrade();
        let inject = (self.trigger)(&event) && !self.fired.swap(true, Ordering::SeqCst);
        let reserved = Arc::clone(&self.reserved);
        let observed = Arc::clone(&self.observed);
        let ready = Arc::clone(&self.ready);
        let proceed = Arc::clone(&self.proceed);
        let pause = self.pause;
        let cancel = self.cancel;
        let injected_message = self.message.clone();
        let send = self.send;
        let pause_after_delivery = self.pause_after_delivery;
        let delivered = Arc::clone(&self.delivered);
        Box::pin(async move {
            observed.lock().unwrap().push(event.clone());
            let Some(harness) = harness else { return };
            if inject {
                // This await is inside the tool's event callback. Waiting for
                // the run to end here would deadlock the tool itself.
                let id = if let Some(options) = send {
                    match harness
                        .send_message(injected_message, options)
                        .await
                        .unwrap()
                    {
                        SendMessageResult::Queued { entry_id, .. } => entry_id,
                        result => panic!("active send must enqueue: {result:?}"),
                    }
                } else {
                    harness.append_message(injected_message).await.unwrap()
                };
                assert!(
                    harness.get_entry(&id).await.unwrap().is_none(),
                    "queued context is not yet history"
                );
                reserved.lock().unwrap().push(id);
                ready.notify_one();
                if cancel {
                    harness.abort().await.unwrap();
                }
                if pause {
                    proceed.notified().await;
                }
            }
            if let AgentEvent::MessageEnd { message, .. } = event {
                if message != context_message() {
                    return;
                }
                let ids = reserved.lock().unwrap().clone();
                for id in ids {
                    assert!(
                        harness.get_entry(&id).await.unwrap().is_some(),
                        "display only after commit"
                    );
                }
                delivered.notify_one();
                if pause_after_delivery {
                    proceed.notified().await;
                }
            }
        })
    }
    fn try_emit(&self, _event: AgentEvent) {}
}

fn on_tool(event: &AgentEvent) -> bool {
    matches!(event, AgentEvent::ToolExecutionEnd { .. })
}
fn on_final(event: &AgentEvent) -> bool {
    matches!(event, AgentEvent::MessageEnd { message: AgentMessage::Assistant(a), .. }
        if !a.content.iter().any(|c| matches!(c, Content::ToolCall(_))))
}
fn on_start(event: &AgentEvent) -> bool {
    matches!(event, AgentEvent::ToolExecutionStart { .. })
}

fn fresh_session() -> Session {
    named_session("context-order")
}

fn named_session(id: &str) -> Session {
    Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: id.into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )),
        None,
    )
}

async fn harness(
    session: Session,
    script: FauxScript,
    emitter: Option<Arc<AppendDuringEvent>>,
    restore: bool,
) -> (Arc<AgentHarness>, Arc<FauxProvider>) {
    harness_with_compaction(session, script, emitter, restore, false).await
}

async fn harness_with_compaction(
    session: Session,
    script: FauxScript,
    emitter: Option<Arc<AppendDuringEvent>>,
    restore: bool,
    compact: bool,
) -> (Arc<AgentHarness>, Arc<FauxProvider>) {
    let provider = FauxProvider::new(script);
    let env = Arc::new(InMemoryExecutionEnv::new());
    let tools = vec![HarnessTool::new(create_write_tool(
        &ExecutionToolContext::new(env.clone(), Some(env)),
    ))];
    let mut model = provider.default_model().clone();
    if compact {
        model.context_window = 120;
    }
    let options = AgentHarnessOptions {
        model,
        thinking_level: Default::default(),
        active_tool_names: vec!["write".into()],
        tools,
        system_prompt: None,
        resources: Default::default(),
        stream_options: Default::default(),
        retry: rpi_harness::types::RetryPolicy {
            base_delay_ms: 1,
            ..Default::default()
        },
        compaction: rpi_harness::types::CompactionSettings {
            enabled: compact,
            reserve_tokens: 20,
            keep_recent_tokens: 1,
        },
        steering_mode: Default::default(),
        follow_up_mode: Default::default(),
        tool_execution: Default::default(),
        drive: Default::default(),
        session,
        allow_existing_session: restore,
        models: vec![provider.clone() as Arc<dyn Provider>],
        to_provider_messages: None,
        entry_projectors: Default::default(),
        agent_emitter: emitter.clone().map(|e| e as Arc<dyn AgentEmitter>),
        before_tool_call: None,
        after_tool_call: None,
        transform_context: None,
        entry_transforms: Vec::new(),
        provider_hooks: None,
    };
    let harness = Arc::new(AgentHarness::create(options).await.unwrap());
    if let Some(emitter) = emitter {
        *emitter.harness.lock().unwrap() = Arc::downgrade(&harness);
    }
    (harness, provider)
}

async fn history(harness: &AgentHarness, lane: &str) -> Vec<Entry> {
    harness
        .session()
        .view(lane)
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &Default::default(),
        )
        .await
        .unwrap()
}

async fn validate_history(harness: &AgentHarness) {
    let session = harness.session();
    rpi_harness::session::validate_record_log(&rpi_harness::session::RecordLogSlice {
        lane: "main".into(),
        open_operations: session.find_open_operations("main", None).await.unwrap(),
        records: session
            .find_records(&RecordQuery {
                lane: Some("main".into()),
                ..Default::default()
            })
            .await
            .unwrap(),
        entries: session.find_entries(&EntryQuery::default()).await.unwrap(),
    })
    .unwrap();
}

fn tool_script() -> FauxScript {
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::message(faux_assistant_message(
            vec![
                FauxBlock::tool_call("write", serde_json::json!({"path":"a.txt","content":"a"})),
                FauxBlock::tool_call("write", serde_json::json!({"path":"b.txt","content":"b"})),
            ],
            StopReason::ToolUse,
        )),
        FauxStep::factory(|ctx| {
            assert_eq!(ctx.messages.len(), 5);
            assert!(matches!(&ctx.messages[1], Message::Assistant(_)));
            assert!(matches!(&ctx.messages[2], Message::ToolResult(_)));
            assert!(matches!(&ctx.messages[3], Message::ToolResult(_)));
            assert!(matches!(&ctx.messages[4], Message::User(_)));
            faux_assistant_message("done", StopReason::Stop)
        }),
    ]);
    script
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_callback_append_waits_for_the_whole_batch_and_reaches_the_next_request() {
    let emitter = Arc::new(AppendDuringEvent::new(on_tool));
    let (h, provider) = harness(fresh_session(), tool_script(), Some(emitter.clone()), false).await;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h.prompt_text("write two files", vec![]),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 2);
    let entries = history(&h, "main").await;
    assert_eq!(entries.len(), 6);
    assert!(
        matches!(&entries[2], Entry::Message(m) if matches!(m.message, AgentMessage::ToolResult(_)))
    );
    assert!(
        matches!(&entries[3], Entry::Message(m) if matches!(m.message, AgentMessage::ToolResult(_)))
    );
    assert_eq!(entries[4].id(), emitter.reserved.lock().unwrap()[0]);
    let events = emitter.observed.lock().unwrap();
    let last_result = events
        .iter()
        .rposition(|e| {
            matches!(
                e,
                AgentEvent::MessageEnd {
                    message: AgentMessage::ToolResult(_),
                    ..
                }
            )
        })
        .unwrap();
    let context = events
        .iter()
        .position(|e| {
            matches!(
                e,
                AgentEvent::MessageEnd {
                    message: AgentMessage::Custom(_),
                    ..
                }
            )
        })
        .unwrap();
    assert!(last_result < context);
    drop(events);
    validate_history(&h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn append_during_a_final_response_preserves_final_id_without_starting_another_turn() {
    let emitter = Arc::new(AppendDuringEvent::new(on_final));
    let (h, provider) = harness(
        fresh_session(),
        FauxScript::new().with_text("done"),
        Some(emitter.clone()),
        false,
    )
    .await;
    let result = h.prompt_text("answer", vec![]).await.unwrap();
    let entries = history(&h, "main").await;
    assert_eq!(entries.len(), 3);
    assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 1);
    let HarnessRunOutcome::Completed {
        final_entry_id,
        leaf_id,
        ..
    } = result.outcome
    else {
        panic!("completed")
    };
    assert_eq!(final_entry_id, entries[1].id());
    assert_eq!(leaf_id, entries[2].id());
    assert_eq!(entries[2].id(), emitter.reserved.lock().unwrap()[0]);
    validate_history(&h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn accepted_context_is_flushed_after_cancelled_tool_results() {
    let mut emitter = AppendDuringEvent::new(on_start);
    emitter.cancel = true;
    let emitter = Arc::new(emitter);
    let (h, _) = harness(
        fresh_session(),
        FauxScript::new()
            .with_tool_call("write", serde_json::json!({"path":"a.txt","content":"a"})),
        Some(emitter.clone()),
        false,
    )
    .await;
    let result = h.prompt_text("write", vec![]).await.unwrap();
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Aborted { .. } | HarnessRunOutcome::Completed { .. }
    ));
    let entries = history(&h, "main").await;
    let result_at = entries
        .iter()
        .position(
            |e| matches!(e, Entry::Message(m) if matches!(m.message, AgentMessage::ToolResult(_))),
        )
        .unwrap();
    let context_at = entries
        .iter()
        .position(|e| e.id() == emitter.reserved.lock().unwrap()[0])
        .unwrap();
    assert!(result_at < context_at);
    assert!(
        matches!(&entries[result_at], Entry::Message(m) if matches!(&m.message, AgentMessage::ToolResult(t) if t.is_error))
    );
    validate_history(&h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acknowledged_context_survives_a_crash_and_is_not_duplicated_on_a_second_restore() {
    let mut emitter = AppendDuringEvent::new(on_start);
    emitter.pause = true;
    let emitter = Arc::new(emitter);
    use rpi_harness::session::jsonl::{
        JsonlSessionCreateOptions, JsonlSessionRepo, JsonlSessionRepoOptions, JsonlSessionStorage,
    };
    let fs = Arc::new(InMemoryExecutionEnv::with_cwd("/".into()));
    let clock = Arc::new(SystemClock);
    let ids = Arc::new(DefaultIdGenerator::new());
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: fs.clone(),
        sessions_root: "/sessions".into(),
        clock: clock.clone(),
        ids: ids.clone(),
    });
    let storage = repo
        .create_typed(&JsonlSessionCreateOptions {
            id: Some("context-order-disk".into()),
            parent_session_id: None,
            cwd: "/project".into(),
            metadata: None,
        })
        .await
        .unwrap();
    let path = storage.jsonl_metadata().path;
    let session = Session::new(Arc::new(storage), None);
    let (h, _) = harness(
        session.clone(),
        FauxScript::new()
            .with_tool_call("write", serde_json::json!({"path":"a.txt","content":"a"})),
        Some(emitter.clone()),
        false,
    )
    .await;
    let run = {
        let h = h.clone();
        tokio::spawn(async move { h.prompt_text("write", vec![]).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), emitter.ready.notified())
        .await
        .unwrap();
    let id = emitter.reserved.lock().unwrap()[0].clone();
    run.abort();
    let _ = run.await;
    drop(h);
    let session = Session::new(
        Arc::new(
            JsonlSessionStorage::load(fs.clone(), &path, clock.clone(), ids.clone())
                .await
                .unwrap(),
        ),
        None,
    );
    let (restored, _) = harness(session.clone(), FauxScript::new(), None, true).await;
    let entries = history(&restored, "main").await;
    let context_at = entries.iter().position(|e| e.id() == id).unwrap();
    assert!(
        matches!(&entries[context_at - 1], Entry::Message(m) if matches!(m.message, AgentMessage::ToolResult(_)))
    );
    assert_eq!(entries.iter().filter(|e| e.id() == id).count(), 1);
    validate_history(&restored).await;
    drop(restored);
    let session = Session::new(
        Arc::new(
            JsonlSessionStorage::load(fs, &path, clock, ids)
                .await
                .unwrap(),
        ),
        None,
    );
    let (restored, _) = harness(session, FauxScript::new(), None, true).await;
    assert_eq!(
        history(&restored, "main")
            .await
            .iter()
            .filter(|e| e.id() == id)
            .count(),
        1
    );
    validate_history(&restored).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn context_appends_are_fifo_and_do_not_leak_between_lanes() {
    let mut emitter = AppendDuringEvent::new(on_start);
    emitter.pause = true;
    let emitter = Arc::new(emitter);
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::tool_call("write", serde_json::json!({"path":"a.txt","content":"a"})),
        FauxStep::factory(|ctx| {
            assert_eq!(ctx.messages.len(), 5);
            assert!(matches!(&ctx.messages[2], Message::ToolResult(_)));
            assert!(matches!(&ctx.messages[3], Message::User(_)));
            assert!(matches!(&ctx.messages[4], Message::User(_)));
            assert!(!serde_json::to_string(&ctx.messages)
                .unwrap()
                .contains("side only"));
            faux_assistant_message("done", StopReason::Stop)
        }),
    ]);
    let (h, _) = harness(fresh_session(), script, Some(emitter.clone()), false).await;
    h.session().create_lane("side", None).await.unwrap();
    let run = {
        let h = h.clone();
        tokio::spawn(async move { h.prompt_text("write", vec![]).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), emitter.ready.notified())
        .await
        .unwrap();
    let second = h.append_message(context_message()).await.unwrap();
    let side = h
        .lane("side")
        .append_message(create_custom_message(
            "goal",
            UserContent::Text("side only".into()),
            true,
            None,
            0,
        ))
        .await
        .unwrap();
    assert_eq!(history(&h, "side").await.len(), 1);
    assert_eq!(history(&h, "side").await[0].id(), side);
    emitter.proceed.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    let entries = history(&h, "main").await;
    assert_eq!(entries[3].id(), emitter.reserved.lock().unwrap()[0]);
    assert_eq!(entries[4].id(), second);
    assert!(!entries.iter().any(|e| e.id() == side));
    validate_history(&h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_keeps_committed_context_and_does_not_duplicate_the_tool_batch() {
    let emitter = Arc::new(AppendDuringEvent::new(on_tool));
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::tool_call("write", serde_json::json!({"path":"a.txt","content":"a"})),
        FauxStep::factory(|ctx| {
            assert_eq!(ctx.messages.len(), 4);
            let mut failure = faux_assistant_message("", StopReason::Error);
            failure.error_message = Some("503 service unavailable".into());
            failure
        }),
        FauxStep::factory(|ctx| {
            assert_eq!(
                ctx.messages.len(),
                4,
                "retry must keep the committed context"
            );
            assert!(matches!(&ctx.messages[2], Message::ToolResult(_)));
            faux_assistant_message("recovered", StopReason::Stop)
        }),
    ]);
    let (h, provider) = harness(fresh_session(), script, Some(emitter.clone()), false).await;
    let result = h.prompt_text("write", vec![]).await.unwrap();
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 3);
    let entries = history(&h, "main").await;
    assert_eq!(entries.len(), 5);
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.id() == emitter.reserved.lock().unwrap()[0])
            .count(),
        1
    );
    validate_history(&h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compaction_sees_accepted_context_only_after_complete_tool_results() {
    let mut emitter = AppendDuringEvent::new(on_tool);
    emitter.message = create_custom_message(
        "goal",
        UserContent::Text("goal state ".repeat(500)),
        true,
        None,
        0,
    );
    let emitter = Arc::new(emitter);
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::tool_call("write", serde_json::json!({"path":"a.txt","content":"a"})),
        FauxStep::factory(|ctx| {
            let serialized = serde_json::to_string(&ctx.messages).unwrap();
            // Recent goal context can be retained rather than summarized.
            assert!(
                serialized.contains("write"),
                "compaction must include the settled tool batch"
            );
            faux_assistant_message("## Goal\nKeep goal state and continue.", StopReason::Stop)
        }),
        FauxStep::factory(|ctx| {
            let serialized = serde_json::to_string(&ctx.messages).unwrap();
            assert!(serialized.contains("goal state"));
            faux_assistant_message("done", StopReason::Stop)
        }),
    ]);
    let (h, provider) =
        harness_with_compaction(fresh_session(), script, Some(emitter.clone()), false, true).await;
    let result = h.prompt_text("write", vec![]).await.unwrap();
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 3);
    let entries = history(&h, "main").await;
    let context_at = entries
        .iter()
        .position(|e| e.id() == emitter.reserved.lock().unwrap()[0])
        .unwrap();
    let compaction_at = entries
        .iter()
        .position(|e| matches!(e, Entry::Compaction(_)))
        .unwrap();
    assert!(context_at < compaction_at);
    let Entry::Compaction(compaction) = &entries[compaction_at] else {
        unreachable!()
    };
    assert!(
        serde_json::to_string(&compaction.retained_tail)
            .unwrap()
            .contains("goal state"),
        "recent context must survive in the retained tail"
    );
    validate_history(&h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_message_idle_defaults_and_false_append_without_model_calls() {
    let (h, provider) = harness(fresh_session(), FauxScript::new(), None, false).await;
    for trigger_turn in [None, Some(false)] {
        for deliver_as in [MessageDelivery::Steer, MessageDelivery::FollowUp] {
            let result = h
                .send_message(
                    context_message(),
                    SendMessageOptions {
                        trigger_turn,
                        deliver_as,
                    },
                )
                .await
                .unwrap();
            let SendMessageResult::Appended { entry_id } = result else {
                panic!("idle context")
            };
            assert!(h.get_entry(&entry_id).await.unwrap().is_some());
        }
    }
    assert_eq!(history(&h, "main").await.len(), 4);
    assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_message_explicit_trigger_while_idle_drives_a_run() {
    let (h, provider) = harness(
        fresh_session(),
        FauxScript::new().with_text("done"),
        None,
        false,
    )
    .await;
    let result = h
        .lane("main")
        .send_message(
            context_message(),
            SendMessageOptions {
                trigger_turn: Some(true),
                deliver_as: MessageDelivery::FollowUp,
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(result, SendMessageResult::Run(run) if matches!(run.outcome, HarnessRunOutcome::Completed { .. }))
    );
    assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 1);
    assert_eq!(history(&h, "main").await.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_message_default_while_running_steers_without_reentrant_run() {
    let mut emitter = AppendDuringEvent::new(on_tool);
    emitter.send = Some(SendMessageOptions::default());
    let emitter = Arc::new(emitter);
    let (h, provider) = harness(fresh_session(), tool_script(), Some(emitter.clone()), false).await;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h.prompt_text("write", vec![]),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 2);
    let entries = history(&h, "main").await;
    assert_eq!(entries.len(), 6);
    assert_eq!(entries[4].id(), emitter.reserved.lock().unwrap()[0]);
    validate_history(&h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_message_follow_up_waits_for_completion_and_preserves_assistant_order() {
    let mut emitter = AppendDuringEvent::new(on_tool);
    emitter.send = Some(SendMessageOptions {
        trigger_turn: None,
        deliver_as: MessageDelivery::FollowUp,
    });
    let emitter = Arc::new(emitter);
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::tool_call("write", serde_json::json!({"path":"a.txt","content":"a"})),
        FauxStep::factory(|ctx| {
            assert_eq!(ctx.messages.len(), 3);
            assert!(!serde_json::to_string(&ctx.messages)
                .unwrap()
                .contains("goal state"));
            faux_assistant_message("first task done", StopReason::Stop)
        }),
        FauxStep::factory(|ctx| {
            assert_eq!(ctx.messages.len(), 5);
            assert!(matches!(&ctx.messages[3], Message::Assistant(_)));
            assert!(matches!(&ctx.messages[4], Message::User(_)));
            faux_assistant_message("follow-up done", StopReason::Stop)
        }),
    ]);
    let (h, provider) = harness(fresh_session(), script, Some(emitter.clone()), false).await;
    let result = h.prompt_text("write", vec![]).await.unwrap();
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 3);
    let entries = history(&h, "main").await;
    assert_eq!(entries.len(), 6);
    assert!(
        matches!(&entries[3], Entry::Message(m) if matches!(m.message, AgentMessage::Assistant(_)))
    );
    assert_eq!(entries[4].id(), emitter.reserved.lock().unwrap()[0]);
    validate_history(&h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_message_false_while_running_ignores_follow_up_and_only_updates_context() {
    let mut emitter = AppendDuringEvent::new(on_tool);
    emitter.send = Some(SendMessageOptions {
        trigger_turn: Some(false),
        deliver_as: MessageDelivery::FollowUp,
    });
    let emitter = Arc::new(emitter);
    let (h, provider) = harness(fresh_session(), tool_script(), Some(emitter.clone()), false).await;
    h.prompt_text("write", vec![]).await.unwrap();
    assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 2);
    assert!(h.queued_messages().await.unwrap().is_empty());
    assert_eq!(
        history(&h, "main").await[4].id(),
        emitter.reserved.lock().unwrap()[0]
    );
    validate_history(&h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn next_turn_takes_precedence_over_trigger_and_keeps_the_reserved_entry_id() {
    for trigger_turn in [None, Some(false), Some(true)] {
        let script = FauxScript::new();
        script.set_responses(vec![FauxStep::factory(|ctx| {
            assert_eq!(ctx.messages.len(), 2);
            assert!(serde_json::to_string(&ctx.messages[0])
                .unwrap()
                .contains("goal state"));
            faux_assistant_message("done", StopReason::Stop)
        })]);
        let (h, provider) = harness(fresh_session(), script, None, false).await;
        let result = h
            .send_message(
                context_message(),
                SendMessageOptions {
                    trigger_turn,
                    deliver_as: MessageDelivery::NextTurn,
                },
            )
            .await
            .unwrap();
        let SendMessageResult::Queued { entry_id, queue } = result else {
            panic!("next turn")
        };
        assert_eq!(queue, rpi_harness::session::types::QueueKind::NextRun);
        assert!(h.get_entry(&entry_id).await.unwrap().is_none());
        assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 0);
        h.prompt_text("next user prompt", vec![]).await.unwrap();
        assert_eq!(history(&h, "main").await[0].id(), entry_id);
        validate_history(&h).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn next_turn_survives_restart_and_is_not_consumed_by_automatic_resume() {
    let mut emitter = AppendDuringEvent::new(on_start);
    emitter.send = Some(SendMessageOptions {
        trigger_turn: Some(true),
        deliver_as: MessageDelivery::NextTurn,
    });
    emitter.pause = true;
    let emitter = Arc::new(emitter);
    let session = fresh_session();
    let (h, _) = harness(
        session.clone(),
        FauxScript::new()
            .with_tool_call("write", serde_json::json!({"path":"a.txt","content":"a"})),
        Some(emitter.clone()),
        false,
    )
    .await;
    let run = {
        let h = h.clone();
        tokio::spawn(async move { h.prompt_text("write", vec![]).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), emitter.ready.notified())
        .await
        .unwrap();
    let id = emitter.reserved.lock().unwrap()[0].clone();
    run.abort();
    let _ = run.await;
    drop(h);
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::factory(|ctx| {
            assert!(!serde_json::to_string(&ctx.messages)
                .unwrap()
                .contains("goal state"));
            faux_assistant_message("recovered", StopReason::Stop)
        }),
        FauxStep::factory(|ctx| {
            assert!(serde_json::to_string(&ctx.messages)
                .unwrap()
                .contains("goal state"));
            faux_assistant_message("next turn done", StopReason::Stop)
        }),
    ]);
    let (h, _) = harness(session.clone(), script, None, true).await;
    assert!(h.resume_pending().await.unwrap().is_some());
    assert!(h.get_entry(&id).await.unwrap().is_none());
    h.prompt_text("next user prompt", vec![]).await.unwrap();
    assert_eq!(
        history(&h, "main")
            .await
            .iter()
            .filter(|e| e.id() == id)
            .count(),
        1
    );
    validate_history(&h).await;
    drop(h);
    let (h, _) = harness(session, FauxScript::new(), None, true).await;
    assert_eq!(
        history(&h, "main")
            .await
            .iter()
            .filter(|e| e.id() == id)
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn send_message_next_turn_is_scoped_to_the_selected_lane() {
    let (h, provider) = harness(
        fresh_session(),
        FauxScript::new()
            .with_text("main done")
            .with_text("side done"),
        None,
        false,
    )
    .await;
    h.session().create_lane("side", None).await.unwrap();
    let result = h
        .lane("side")
        .send_message(
            context_message(),
            SendMessageOptions {
                trigger_turn: Some(true),
                deliver_as: MessageDelivery::NextTurn,
            },
        )
        .await
        .unwrap();
    let SendMessageResult::Queued { entry_id, .. } = result else {
        panic!("queued")
    };
    h.prompt_text("main prompt", vec![]).await.unwrap();
    assert!(h.get_entry(&entry_id).await.unwrap().is_none());
    h.lane("side")
        .prompt_text("side prompt", vec![])
        .await
        .unwrap();
    assert_eq!(history(&h, "side").await[0].id(), entry_id);
    assert!(!history(&h, "main").await.iter().any(|e| e.id() == entry_id));
    assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_resumes_delivered_steering_but_not_silent_context_after_a_final_reply() {
    for trigger_turn in [None, Some(false)] {
        let mut emitter = AppendDuringEvent::new(on_final);
        emitter.send = Some(SendMessageOptions {
            trigger_turn,
            deliver_as: MessageDelivery::Steer,
        });
        emitter.pause_after_delivery = true;
        let emitter = Arc::new(emitter);
        let session = fresh_session();
        let (h, _) = harness(
            session.clone(),
            FauxScript::new().with_text("first reply"),
            Some(emitter.clone()),
            false,
        )
        .await;
        let run = {
            let h = h.clone();
            tokio::spawn(async move { h.prompt_text("answer", vec![]).await })
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            emitter.delivered.notified(),
        )
        .await
        .unwrap();
        let id = emitter.reserved.lock().unwrap()[0].clone();
        run.abort();
        let _ = run.await;
        drop(h);
        let (h, provider) = harness(
            session.clone(),
            FauxScript::new().with_text("continued"),
            None,
            true,
        )
        .await;
        assert!(
            h.queued_messages().await.unwrap().is_empty(),
            "committed injections must not be queued again"
        );
        let resumed = h.resume_pending().await.unwrap();
        assert_eq!(resumed.is_some(), trigger_turn != Some(false));
        assert_eq!(
            provider.state().call_count.load(Ordering::SeqCst),
            usize::from(trigger_turn != Some(false))
        );
        let entries = history(&h, "main").await;
        assert_eq!(entries.iter().filter(|e| e.id() == id).count(), 1);
        assert!(
            matches!(&entries[1], Entry::Message(m) if matches!(m.message, AgentMessage::Assistant(_)))
        );
        validate_history(&h).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_during_retry_does_not_duplicate_committed_tool_batch() {
    let mut emitter =
        AppendDuringEvent::new(|event| matches!(event, AgentEvent::RetryScheduled { .. }));
    emitter.cancel = true;
    let emitter = Arc::new(emitter);
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::tool_call("write", serde_json::json!({"path":"a.txt","content":"a"})),
        FauxStep::factory(|_| {
            let mut failure = faux_assistant_message("", StopReason::Error);
            failure.error_message = Some("503 service unavailable".into());
            failure
        }),
    ]);
    let (h, _) = harness(fresh_session(), script, Some(emitter), false).await;
    h.prompt_text("write", vec![]).await.unwrap();
    let entries = history(&h, "main").await;
    let tool_results = entries.iter().filter(|entry| matches!(entry, Entry::Message(m) if matches!(m.message, AgentMessage::ToolResult(_)))).count();
    assert_eq!(
        tool_results, 1,
        "committed tool result must not be written twice: {entries:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_after_silent_context_does_not_replay_committed_final_assistant() {
    let mut emitter = AppendDuringEvent::new(on_final);
    emitter.send = Some(SendMessageOptions {
        trigger_turn: Some(false),
        deliver_as: MessageDelivery::Steer,
    });
    emitter.pause_after_delivery = true;
    let emitter = Arc::new(emitter);
    let session = fresh_session();
    let (h, _) = harness(
        session.clone(),
        FauxScript::new().with_text("first reply"),
        Some(emitter.clone()),
        false,
    )
    .await;
    let run = {
        let h = h.clone();
        tokio::spawn(async move { h.prompt_text("answer", vec![]).await })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        emitter.delivered.notified(),
    )
    .await
    .unwrap();
    run.abort();
    let _ = run.await;
    drop(h);
    let (restored, _) = harness(session, FauxScript::new(), None, true).await;
    let entries = history(&restored, "main").await;
    let assistants = entries.iter().filter(|entry| matches!(entry, Entry::Message(m) if matches!(m.message, AgentMessage::Assistant(_)))).count();
    assert_eq!(
        assistants, 1,
        "already committed final response must not be salvaged again: {entries:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compaction_then_retry_preserves_the_recovered_final_message() {
    let mut emitter = AppendDuringEvent::new(on_tool);
    emitter.message = create_custom_message(
        "goal",
        UserContent::Text("goal state ".repeat(500)),
        true,
        None,
        0,
    );
    let emitter = Arc::new(emitter);
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::tool_call("write", serde_json::json!({"path":"a.txt","content":"a"})),
        FauxStep::message(faux_assistant_message(
            "## Goal\nContinue.",
            StopReason::Stop,
        )),
        FauxStep::factory(|_| {
            let mut failure = faux_assistant_message("", StopReason::Error);
            failure.error_message = Some("503 service unavailable".into());
            failure
        }),
        FauxStep::message(faux_assistant_message("recovered final", StopReason::Stop)),
    ]);
    let (h, provider) =
        harness_with_compaction(fresh_session(), script, Some(emitter), false, true).await;
    let result = h.prompt_text("write", vec![]).await.unwrap();
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 4);
    let entries = history(&h, "main").await;
    assert!(entries.iter().any(|entry| matches!(entry, Entry::Message(m) if matches!(&m.message, AgentMessage::Assistant(a) if a.content.iter().any(|c| matches!(c, Content::Text(t) if t.text == "recovered final"))))), "successful retry reply must be persisted: {entries:?}");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_success_with_silent_context_must_not_schedule_another_resume() {
    for failed_attempts in [1, 2] {
        let mut emitter = AppendDuringEvent::new(
            |event| matches!(event, AgentEvent::MessageEnd { message: AgentMessage::Assistant(a), .. } if a.stop_reason == StopReason::Stop),
        );
        emitter.send = Some(SendMessageOptions {
            trigger_turn: Some(false),
            deliver_as: MessageDelivery::Steer,
        });
        emitter.pause_after_delivery = true;
        let emitter = Arc::new(emitter);
        let script = FauxScript::new();
        let mut failed = faux_assistant_message("", StopReason::Error);
        failed.error_message = Some("503 service unavailable".into());
        let mut responses: Vec<_> = (0..failed_attempts)
            .map(|_| FauxStep::message(failed.clone()))
            .collect();
        responses.push(FauxStep::text("retry succeeded"));
        script.set_responses(responses);
        let session = fresh_session();
        let (h, _) = harness(session.clone(), script, Some(emitter.clone()), false).await;
        let run = {
            let h = h.clone();
            tokio::spawn(async move { h.prompt_text("answer", vec![]).await })
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            emitter.delivered.notified(),
        )
        .await
        .unwrap();
        run.abort();
        let _ = run.await;
        drop(h);
        let (restored, provider) = harness(
            session,
            FauxScript::new().with_text("unwanted second reply"),
            None,
            true,
        )
        .await;
        let resumed = restored.resume_pending().await.unwrap();
        assert!(resumed.is_none() && provider.state().call_count.load(Ordering::SeqCst) == 0, "a committed successful reply plus silent context must not request another turn; provider calls: {}", provider.state().call_count.load(Ordering::SeqCst));
        assert_eq!(history(&restored, "main").await.iter().filter(|entry| matches!(entry, Entry::Message(m) if matches!(m.message, AgentMessage::Assistant(_)))).count(), 1, "failed generations must not reappear in history");
        validate_history(&restored).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_stored_assistant_keeps_its_reserved_entry_id() {
    let mut emitter = AppendDuringEvent::new(on_tool);
    emitter.message = AgentMessage::Assistant(Box::new(faux_assistant_message(
        "extension assistant",
        StopReason::Stop,
    )));
    emitter.send = Some(SendMessageOptions::default());
    let emitter = Arc::new(emitter);
    let script = FauxScript::new()
        .with_tool_call("write", serde_json::json!({"path":"a.txt", "content":"a"}))
        .with_text("done");
    let (h, _) = harness(fresh_session(), script, Some(emitter.clone()), false).await;
    h.prompt_text("write", vec![]).await.unwrap();
    let id = emitter.reserved.lock().unwrap()[0].clone();
    let committed = h.get_entry(&id).await.unwrap().is_some();
    let session = h.session().clone();
    drop(h);
    let (restored, _) = harness(session, FauxScript::new(), None, true).await;
    let queued = restored.queued_messages().await.unwrap();
    assert!(
        committed && queued.is_empty(),
        "accepted assistant reservation committed: {committed}; queued again after restart: {}",
        queued.steering.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_frames_are_salvaged_after_generation_has_started() {
    let mut emitter = AppendDuringEvent::new(|event| {
        matches!(event,
        AgentEvent::MessageUpdate { assistant_message_event: rpi_ai::types::AssistantMessageEvent::TextDelta { partial, .. }, .. }
            if partial.content.iter().any(|content| matches!(content, Content::Text(text) if text.text == "retry partial")))
    });
    emitter.send = Some(SendMessageOptions {
        trigger_turn: Some(false),
        deliver_as: MessageDelivery::Steer,
    });
    emitter.pause = true;
    let emitter = Arc::new(emitter);
    let mut failed = faux_assistant_message("failed generation", StopReason::Error);
    failed.error_message = Some("503 service unavailable".into());
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::message(failed),
        FauxStep::text("retry partial"),
    ]);
    let session = fresh_session();
    let (h, _) = harness(session.clone(), script, Some(emitter.clone()), false).await;
    let run = {
        let h = h.clone();
        tokio::spawn(async move { h.prompt_text("answer", vec![]).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), emitter.ready.notified())
        .await
        .unwrap();
    // Delivery only queues the frame for batched persistence. Crash after a
    // recoverable checkpoint, rather than racing the background frame writer.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let frames = session
                .find_records(&RecordQuery {
                    record_type: Some("assistant_frame"),
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                })
                .await
                .unwrap();
            if let Some(LaneRecord::AssistantFrame(frame)) = frames.last() {
                let recovered =
                    rpi_harness::frame_progress::salvage_run_frames(&session, &frame.run_id)
                        .await
                        .unwrap();
                if recovered.iter().any(|message| {
                    message.content.iter().any(
                        |content| matches!(content, Content::Text(text) if text.text == "retry partial"),
                    )
                }) {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("retry progress must reach the persisted recovery checkpoint");
    run.abort();
    let _ = run.await;
    drop(h);
    let (restored, _) = harness(session, FauxScript::new(), None, true).await;
    let assistants: Vec<_> = history(&restored, "main")
        .await
        .into_iter()
        .filter_map(|entry| {
            let Entry::Message(m) = entry else {
                return None;
            };
            let AgentMessage::Assistant(a) = m.message else {
                return None;
            };
            Some(a)
        })
        .collect();
    assert_eq!(
        assistants.len(),
        1,
        "only the begun retry's progress is recovered"
    );
    assert!(assistants[0]
        .content
        .iter()
        .any(|content| matches!(content, Content::Text(t) if t.text == "retry partial")));
    validate_history(&restored).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_accepted_message_roles_keep_delivery_ids_across_restart() {
    let messages = vec![
        AgentMessage::User(rpi_ai::types::UserMessage::new("injected user", 0)),
        context_message(),
        AgentMessage::Assistant(Box::new(faux_assistant_message(
            "injected assistant",
            StopReason::Stop,
        ))),
        AgentMessage::ToolResult(Box::new(rpi_ai::types::ToolResultMessage {
            role: rpi_ai::types::ToolResultRole,
            tool_call_id: "extension-result".into(),
            tool_name: "extension".into(),
            content: vec![Content::text("injected result")],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: 0,
        })),
    ];
    for deliver_as in [MessageDelivery::Steer, MessageDelivery::FollowUp] {
        for message in &messages {
            let mut emitter = AppendDuringEvent::new(on_tool);
            emitter.message = message.clone();
            emitter.send = Some(SendMessageOptions {
                trigger_turn: None,
                deliver_as,
            });
            let emitter = Arc::new(emitter);
            let script = FauxScript::new()
                .with_tool_call("write", serde_json::json!({"path":"a.txt", "content":"a"}))
                .with_text("first reply")
                .with_text("follow-up reply");
            let (h, _) = harness(fresh_session(), script, Some(emitter.clone()), false).await;
            h.prompt_text("write", vec![]).await.unwrap();
            let id = emitter.reserved.lock().unwrap()[0].clone();
            assert!(
                matches!(h.get_entry(&id).await.unwrap(), Some(Entry::Message(m)) if m.message == *message)
            );
            assert_eq!(
                history(&h, "main")
                    .await
                    .iter()
                    .filter(|entry| entry.id() == id)
                    .count(),
                1
            );
            validate_history(&h).await;
            let session = h.session().clone();
            drop(h);
            let (restored, _) = harness(session, FauxScript::new(), None, true).await;
            assert!(
                restored.queued_messages().await.unwrap().is_empty(),
                "already delivered {deliver_as:?}: {message:?}"
            );
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_switch_does_not_transfer_or_ignore_next_turn_messages() {
    let (h, _) = harness(
        fresh_session(),
        FauxScript::new().with_text("done"),
        None,
        false,
    )
    .await;
    let queued = h
        .send_message(
            create_custom_message(
                "goal",
                UserContent::Text("old session only".into()),
                true,
                None,
                0,
            ),
            SendMessageOptions {
                trigger_turn: None,
                deliver_as: MessageDelivery::NextTurn,
            },
        )
        .await
        .unwrap();
    let SendMessageResult::Queued {
        entry_id: old_id, ..
    } = queued
    else {
        panic!("queued")
    };
    let target_session = Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "target-session".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )),
        None,
    );
    let (target_harness, _) = harness(target_session.clone(), FauxScript::new(), None, false).await;
    let queued = target_harness
        .send_message(
            context_message(),
            SendMessageOptions {
                trigger_turn: None,
                deliver_as: MessageDelivery::NextTurn,
            },
        )
        .await
        .unwrap();
    let SendMessageResult::Queued {
        entry_id: target_id,
        ..
    } = queued
    else {
        panic!("queued")
    };
    drop(target_harness);
    h.set_session(target_session).await.unwrap();
    h.prompt_text("new session prompt", vec![]).await.unwrap();
    let old_leaked = h.get_entry(&old_id).await.unwrap().is_some();
    let target_delivered = h.get_entry(&target_id).await.unwrap().is_some();
    assert!(!old_leaked && target_delivered, "old-session message leaked: {old_leaked}; target-session pending message delivered: {target_delivered}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_completion_does_not_report_another_runs_leaf() {
    let mut emitter = AppendDuringEvent::new(|event| matches!(event, AgentEvent::AgentEnd { .. }));
    emitter.pause_after_delivery = true;
    let emitter = Arc::new(emitter);
    let (h, _) = harness(
        fresh_session(),
        FauxScript::new()
            .with_text("first reply")
            .with_text("second reply"),
        Some(emitter.clone()),
        false,
    )
    .await;
    let ends = Arc::new(Mutex::new(Vec::new()));
    let seen_ends = ends.clone();
    let _listener = h
        .events()
        .on::<rpi_harness::events::RunEndEvent, _>(move |event| {
            seen_ends.lock().unwrap().push(event.clone());
        });
    let first = {
        let h = h.clone();
        tokio::spawn(async move { h.prompt_text("first prompt", vec![]).await })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        emitter.delivered.notified(),
    )
    .await
    .unwrap();
    let first_expected_leaf = emitter.reserved.lock().unwrap()[0].clone();
    let second = h.prompt_text("second prompt", vec![]).await;
    assert!(
        second.is_err(),
        "end callbacks must finish before admitting another run"
    );
    emitter.proceed.notify_one();
    let first = first.await.unwrap().unwrap();
    let HarnessRunOutcome::Completed { leaf_id, .. } = first.outcome else {
        panic!("completed")
    };
    assert_eq!(leaf_id, first_expected_leaf, "first run must report its own final context entry even if another run is admitted; second result: {second:?}");
    assert_eq!(ends.lock().unwrap()[0].leaf_id, leaf_id);
    let second = h.prompt_text("second prompt", vec![]).await.unwrap();
    assert_eq!(ends.lock().unwrap().len(), 2);
    assert_eq!(ends.lock().unwrap()[1].run_id, second.run_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switching_sessions_restores_all_delivery_queues_and_lanes_on_a_round_trip() {
    let source = named_session("source");
    source.create_lane("aux", None).await.unwrap();
    let pinned_source = Session::new(source.storage(), None);
    let script = FauxScript::new();
    script.set_responses((0..8).map(|_| FauxStep::text("done")).collect());
    let (h, _) = harness(source, script, None, false).await;
    let aux = h.lane("aux");
    let target = named_session("target");
    target.create_lane("aux", None).await.unwrap();
    let (target_h, _) = harness(target.clone(), FauxScript::new(), None, false).await;
    let mut source_ids = Vec::new();
    let mut target_ids = Vec::new();
    for (owner, ids) in [(&h, &mut source_ids), (&target_h, &mut target_ids)] {
        for lane in ["main", "aux"] {
            let handle = owner.lane(lane);
            ids.push(handle.steer(context_message()).await.unwrap().entry_id);
            ids.push(handle.follow_up(context_message()).await.unwrap().entry_id);
            ids.push(handle.next_run(context_message()).await.unwrap().entry_id);
        }
    }
    h.set_session(target).await.unwrap();
    h.prompt_text("target main", vec![]).await.unwrap();
    aux.prompt_text("target aux", vec![]).await.unwrap();
    for (index, id) in target_ids.iter().enumerate() {
        assert!(
            h.get_entry(id).await.unwrap().is_some(),
            "target queue {index} {id}; main={:?}; aux={:?}",
            history(&h, "main")
                .await
                .iter()
                .map(Entry::id)
                .collect::<Vec<_>>(),
            history(&h, "aux")
                .await
                .iter()
                .map(Entry::id)
                .collect::<Vec<_>>()
        );
    }
    for id in &source_ids {
        assert!(h.get_entry(id).await.unwrap().is_none());
    }
    h.set_session(pinned_source).await.unwrap();
    h.prompt_text("source main", vec![]).await.unwrap();
    aux.prompt_text("source aux", vec![]).await.unwrap();
    for id in &source_ids {
        assert!(h.get_entry(id).await.unwrap().is_some());
    }
    for id in &target_ids {
        assert!(h.get_entry(id).await.unwrap().is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_switch_waits_for_the_old_operation_to_finish() {
    let mut emitter = AppendDuringEvent::new(on_start);
    emitter.pause = true;
    let emitter = Arc::new(emitter);
    let (h, _) = harness(
        fresh_session(),
        FauxScript::new().with_tool_call("write", serde_json::json!({"path":"a", "content":"a"})),
        Some(emitter.clone()),
        false,
    )
    .await;
    let run = {
        let h = h.clone();
        tokio::spawn(async move { h.prompt_text("write", vec![]).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), emitter.ready.notified())
        .await
        .unwrap();
    let target = named_session("target");
    assert!(h.set_session(target.clone()).await.is_err());
    assert_eq!(
        h.session().get_metadata().await.unwrap().id,
        "context-order"
    );
    h.abort().await.unwrap();
    emitter.proceed.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h.lane("main").wait_for_idle(),
    )
    .await
    .unwrap()
    .unwrap();
    run.await.unwrap().unwrap();
    h.set_session(target).await.unwrap();
    assert_eq!(h.session().get_metadata().await.unwrap().id, "target");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_switch_does_not_resume_another_sessions_interrupted_run() {
    let mut emitter = AppendDuringEvent::new(on_start);
    emitter.pause = true;
    let emitter = Arc::new(emitter);
    let session = fresh_session();
    let (h, _) = harness(
        session.clone(),
        FauxScript::new().with_tool_call("write", serde_json::json!({"path":"a", "content":"a"})),
        Some(emitter.clone()),
        false,
    )
    .await;
    let run = {
        let h = h.clone();
        tokio::spawn(async move { h.prompt_text("write", vec![]).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), emitter.ready.notified())
        .await
        .unwrap();
    run.abort();
    let _ = run.await;
    drop(h);
    let (restored, provider) = harness(session, FauxScript::new(), None, true).await;
    assert!(restored.pending_resume().is_some());
    restored.set_session(named_session("target")).await.unwrap();
    assert!(restored.pending_resume().is_none());
    assert!(restored.resume_pending().await.unwrap().is_none());
    assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_end_listeners_can_append_without_deadlock_or_changing_the_outcome_snapshot() {
    let (h, _) = harness(
        fresh_session(),
        FauxScript::new().with_text("done").with_text("next reply"),
        None,
        false,
    )
    .await;
    let weak = Arc::downgrade(&h);
    let observed = Arc::new(Mutex::new(None));
    let saved = observed.clone();
    let _listener = h
        .events()
        .on::<rpi_harness::events::RunEndEvent, _>(move |event| {
            let h = weak.upgrade().unwrap();
            let id = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(h.append_message(context_message()))
            })
            .unwrap();
            let queued = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(h.lane("main").steer(context_message()))
            })
            .unwrap();
            *saved.lock().unwrap() = Some((event.clone(), id, queued.entry_id));
        });
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        h.prompt_text("answer", vec![]),
    )
    .await
    .unwrap()
    .unwrap();
    let HarnessRunOutcome::Completed { leaf_id, .. } = result.outcome else {
        panic!("completed")
    };
    let (event, id, queued_id) = observed.lock().unwrap().clone().unwrap();
    assert_eq!(event.leaf_id, leaf_id);
    assert_ne!(
        id, leaf_id,
        "writes caused by RunEnd happen after that terminal snapshot"
    );
    assert!(h.get_entry(&id).await.unwrap().is_some());
    h.lane("main").wait_for_idle().await.unwrap();
    validate_history(&h).await;
    drop(_listener);
    h.prompt_text("next prompt", vec![]).await.unwrap();
    assert!(h.get_entry(&queued_id).await.unwrap().is_some());
    validate_history(&h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_from_run_end_is_a_lane_fact_and_keeps_the_log_valid() {
    let (h, _) = harness(
        fresh_session(),
        FauxScript::new().with_text("done"),
        None,
        false,
    )
    .await;
    let weak = Arc::downgrade(&h);
    let _listener = h
        .events()
        .on::<rpi_harness::events::RunEndEvent, _>(move |_| {
            let h = weak.upgrade().unwrap();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(h.record_usage(
                    rpi_ai::types::Usage::zero(),
                    None,
                    Some(serde_json::json!({"phase": "end"})),
                ))
            })
            .unwrap();
        });
    h.prompt_text("answer", vec![]).await.unwrap();
    let records = h
        .session()
        .find_records(&RecordQuery::default())
        .await
        .unwrap();
    let recorded = records
        .iter()
        .find_map(|record| match record {
            rpi_harness::session::types::LaneRecord::Usage(usage)
                if usage.details == Some(serde_json::json!({"phase": "end"})) =>
            {
                Some(usage)
            }
            _ => None,
        })
        .unwrap();
    assert!(recorded.run_id.is_none());
    validate_history(&h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_before_operation_started_does_not_reference_an_unknown_run() {
    let (h, _) = harness(
        fresh_session(),
        FauxScript::new().with_text("done"),
        None,
        false,
    )
    .await;
    let weak = Arc::downgrade(&h);
    let _listener = h
        .events()
        .on::<rpi_harness::events::RunStartEvent, _>(move |_| {
            let h = weak.upgrade().unwrap();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(h.record_usage(
                    rpi_ai::types::Usage::zero(),
                    None,
                    None,
                ))
            })
            .unwrap();
        });
    h.prompt_text("answer", vec![]).await.unwrap();
    validate_history(&h).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usage_is_associated_only_with_an_open_run_on_its_own_lane() {
    let session = fresh_session();
    session.create_lane("aux", None).await.unwrap();
    let mut emitter = AppendDuringEvent::new(on_start);
    emitter.pause = true;
    let emitter = Arc::new(emitter);
    let (h, _) = harness(
        session,
        FauxScript::new().with_tool_call("write", serde_json::json!({"path":"a", "content":"a"})),
        Some(emitter.clone()),
        false,
    )
    .await;
    let run = {
        let h = h.clone();
        tokio::spawn(async move { h.prompt_text("write", vec![]).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), emitter.ready.notified())
        .await
        .unwrap();
    h.record_usage(
        rpi_ai::types::Usage::zero(),
        None,
        Some(serde_json::json!({"lane": "main"})),
    )
    .await
    .unwrap();
    h.lane("aux")
        .record_usage(
            rpi_ai::types::Usage::zero(),
            None,
            Some(serde_json::json!({"lane": "aux"})),
        )
        .await
        .unwrap();
    emitter.proceed.notify_one();
    let completed = run.await.unwrap().unwrap();
    let records = h
        .session()
        .find_records(&RecordQuery::default())
        .await
        .unwrap();
    for lane in ["main", "aux"] {
        let usage = records
            .iter()
            .find_map(|record| match record {
                rpi_harness::session::types::LaneRecord::Usage(usage)
                    if usage.details == Some(serde_json::json!({"lane": lane})) =>
                {
                    Some(usage)
                }
                _ => None,
            })
            .unwrap();
        let expected = (lane == "main").then_some(completed.run_id.as_str());
        assert_eq!(usage.run_id.as_deref(), expected);
        rpi_harness::session::validate_record_log(&rpi_harness::session::RecordLogSlice {
            lane: lane.into(),
            open_operations: h.session().find_open_operations(lane, None).await.unwrap(),
            records: records
                .iter()
                .filter(|record| record.base().lane == lane)
                .cloned()
                .collect(),
            entries: h
                .session()
                .find_entries(&EntryQuery::default())
                .await
                .unwrap(),
        })
        .unwrap();
    }
}

mod resume_admission {
    use super::*;
    use rpi_harness::error::SessionResult;
    use rpi_harness::session::types::{
        BranchBounds, LanePointer, LaneRecord, LogItem, LogOptions, OperationIntent,
        OperationStartedRecord, ProvisionedEntry, SessionStats, SessionStorage,
    };

    struct PausedStorage {
        inner: Arc<dyn SessionStorage>,
        armed: AtomicBool,
        ready: tokio::sync::Notify,
        proceed: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl SessionStorage for PausedStorage {
        async fn prune_completed_progress(&self) -> SessionResult<usize> {
            self.inner.prune_completed_progress().await
        }

        async fn append_progress(
            &self,
            records: Vec<rpi_harness::session::types::AssistantFrameRecord>,
        ) -> SessionResult<Vec<rpi_harness::session::types::AssistantFrameRecord>> {
            self.inner.append_progress(records).await
        }
        async fn read_progress(
            &self,
            run_id: &str,
        ) -> SessionResult<Vec<rpi_harness::session::types::AssistantFrameRecord>> {
            self.inner.read_progress(run_id).await
        }
        fn metadata(&self) -> SessionMetadata {
            self.inner.metadata()
        }
        async fn get_lanes(&self) -> SessionResult<Vec<LanePointer>> {
            self.inner.get_lanes().await
        }
        async fn create_lane(&self, lane: &str, at: Option<&str>) -> SessionResult<()> {
            self.inner.create_lane(lane, at).await
        }
        async fn move_lane(&self, lane: &str, to: Option<&str>) -> SessionResult<()> {
            self.inner.move_lane(lane, to).await
        }
        async fn append_entry(&self, entry: ProvisionedEntry, lane: &str) -> SessionResult<Entry> {
            self.inner.append_entry(entry, lane).await
        }
        async fn append_record(&self, record: LaneRecord) -> SessionResult<LaneRecord> {
            self.inner.append_record(record).await
        }
        async fn get_entry(&self, id: &str) -> SessionResult<Option<Entry>> {
            self.inner.get_entry(id).await
        }
        async fn find_entries(&self, query: &EntryQuery) -> SessionResult<Vec<Entry>> {
            self.inner.find_entries(query).await
        }
        async fn find_entries_on_branch(
            &self,
            query: &EntryQuery,
            bounds: &BranchBounds,
            start: &str,
        ) -> SessionResult<Vec<Entry>> {
            let result = self
                .inner
                .find_entries_on_branch(query, bounds, start)
                .await;
            if self.armed.swap(false, Ordering::SeqCst) {
                self.ready.notify_one();
                self.proceed.notified().await;
            }
            result
        }
        async fn find_records(&self, query: &RecordQuery) -> SessionResult<Vec<LaneRecord>> {
            self.inner.find_records(query).await
        }
        async fn record_page(
            &self,
            query: &rpi_harness::session::types::RecordPageQuery,
        ) -> SessionResult<rpi_harness::session::types::RecordPage> {
            self.inner.record_page(query).await
        }
        async fn find_open_operations(
            &self,
            lane: &str,
            limit: Option<usize>,
        ) -> SessionResult<Vec<OperationStartedRecord>> {
            self.inner.find_open_operations(lane, limit).await
        }
        async fn get_log(&self, options: &LogOptions) -> SessionResult<Vec<LogItem>> {
            self.inner.get_log(options).await
        }
        async fn get_name(&self) -> SessionResult<Option<String>> {
            self.inner.get_name().await
        }
        async fn set_name(&self, name: Option<&str>) -> SessionResult<()> {
            self.inner.set_name(name).await
        }
        async fn get_label(&self, id: &str) -> SessionResult<Option<String>> {
            self.inner.get_label(id).await
        }
        async fn set_label(&self, id: &str, label: Option<&str>) -> SessionResult<()> {
            self.inner.set_label(id, label).await
        }
        async fn get_stats(&self) -> SessionResult<SessionStats> {
            self.inner.get_stats().await
        }
    }

    async fn interrupted_session() -> Session {
        let source = fresh_session();
        let mut emitter = AppendDuringEvent::new(on_start);
        emitter.pause = true;
        let emitter = Arc::new(emitter);
        let (h, _) = harness(
            source.clone(),
            FauxScript::new()
                .with_tool_call("write", serde_json::json!({"path":"a", "content":"a"})),
            Some(emitter.clone()),
            false,
        )
        .await;
        let run = {
            let h = h.clone();
            tokio::spawn(async move { h.prompt_text("write", vec![]).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), emitter.ready.notified())
            .await
            .unwrap();
        run.abort();
        let _ = run.await;
        drop(h);
        source
    }

    #[tokio::test]
    async fn custom_storage_uses_default_context_fallback_and_propagates_errors() {
        let source = fresh_session();
        source
            .append_message(AgentMessage::User(rpi_ai::types::UserMessage::new(
                "fallback", 0,
            )))
            .await
            .unwrap();
        let storage = PausedStorage {
            inner: source.storage(),
            armed: AtomicBool::new(false),
            ready: Default::default(),
            proceed: Default::default(),
        };
        let leaf = storage.inner.get_lanes().await.unwrap()[0]
            .leaf_id
            .clone()
            .unwrap();
        let context = storage
            .context_on_branch(&BranchBounds::default(), &leaf)
            .await
            .unwrap();
        assert_eq!(context.messages.len(), 1);
        let error = storage
            .context_on_branch(&BranchBounds::default(), "missing")
            .await
            .unwrap_err();
        assert_eq!(error.code, rpi_harness::error::SessionErrorCode::NotFound);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn session_switch_is_rejected_through_resume_preparation_and_chain_is_persisted() {
        let source = interrupted_session().await;
        let storage = Arc::new(PausedStorage {
            inner: source.storage(),
            armed: AtomicBool::new(false),
            ready: Default::default(),
            proceed: Default::default(),
        });
        let script = FauxScript::new();
        script.set_responses(vec![FauxStep::factory(|context| {
            assert!(!context.messages.iter().any(|message| matches!(message,
                Message::User(user) if user.content == UserContent::Text("target question".into()))));
            faux_assistant_message("source answer", StopReason::Stop)
        })]);
        let (restored, provider) =
            harness(Session::new(storage.clone(), None), script, None, true).await;
        let pending = restored.pending_resume().unwrap();
        storage.armed.store(true, Ordering::SeqCst);
        let resume = {
            let h = restored.clone();
            tokio::spawn(async move { h.resume_pending().await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), storage.ready.notified())
            .await
            .unwrap();
        let target = named_session("target");
        target
            .append_message(AgentMessage::User(rpi_ai::types::UserMessage::new(
                "target question",
                0,
            )))
            .await
            .unwrap();
        assert!(restored.set_session(target.clone()).await.is_err());
        assert_eq!(
            restored.session().get_metadata().await.unwrap().id,
            "context-order"
        );
        assert!(restored.resume_pending().await.unwrap().is_none());
        storage.proceed.notify_one();
        let resumed = tokio::time::timeout(std::time::Duration::from_secs(5), resume)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 1);
        let records = restored
            .session()
            .find_records(&RecordQuery::default())
            .await
            .unwrap();
        let data = records
            .iter()
            .find_map(|record| match record {
                LaneRecord::OperationStarted(operation) if operation.base.id == resumed.run_id => {
                    match &operation.intent {
                        OperationIntent::Run { resume_data, .. } => resume_data.as_ref(),
                        _ => None,
                    }
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(
            data.get("resumedFrom"),
            Some(&serde_json::json!(pending.chain_origin_run_id))
        );
        assert_eq!(
            data.get("resumeAttempt"),
            Some(&serde_json::json!(pending.attempt + 1))
        );
        validate_history(&restored).await;
        restored.set_session(target).await.unwrap();
        assert_eq!(history(&restored, "main").await.len(), 1);
        assert!(restored.resume_pending().await.unwrap().is_none());
        assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn legacy_deliveries_during_resume_preparation_are_lane_facts() {
        use rpi_harness::session::types::QueueKind;
        for kind in [QueueKind::Steer, QueueKind::FollowUp, QueueKind::NextRun] {
            let source = interrupted_session().await;
            let storage = Arc::new(PausedStorage {
                inner: source.storage(),
                armed: AtomicBool::new(false),
                ready: Default::default(),
                proceed: Default::default(),
            });
            let (h, _) = harness(
                Session::new(storage.clone(), None),
                FauxScript::new().with_text("done").with_text("next reply"),
                None,
                true,
            )
            .await;
            storage.armed.store(true, Ordering::SeqCst);
            let resume = {
                let h = h.clone();
                tokio::spawn(async move { h.resume_pending().await })
            };
            tokio::time::timeout(std::time::Duration::from_secs(5), storage.ready.notified())
                .await
                .unwrap();
            let queued = match kind {
                QueueKind::Steer => h.steer(context_message()).await.unwrap(),
                QueueKind::FollowUp => h.follow_up(context_message()).await.unwrap(),
                QueueKind::NextRun => h.next_run(context_message()).await.unwrap(),
                QueueKind::Context => unreachable!(),
            };
            let records = h
                .session()
                .find_records(&RecordQuery::default())
                .await
                .unwrap();
            let enqueue = records
                .iter()
                .find_map(|record| match record {
                    LaneRecord::QueueEnqueued(enqueued)
                        if enqueued.target.get("id").and_then(|id| id.as_str())
                            == Some(queued.entry_id.as_str()) =>
                    {
                        Some(enqueued)
                    }
                    _ => None,
                })
                .unwrap();
            assert!(enqueue.run_id.is_none());
            storage.proceed.notify_one();
            tokio::time::timeout(std::time::Duration::from_secs(5), resume)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if kind == QueueKind::NextRun {
                assert!(h.get_entry(&queued.entry_id).await.unwrap().is_none());
                h.prompt_text("explicit next turn", vec![]).await.unwrap();
            }
            assert!(h.get_entry(&queued.entry_id).await.unwrap().is_some());
            validate_history(&h).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn rejected_resume_admission_preserves_the_pending_marker() {
        let source = interrupted_session().await;
        let mut emitter = AppendDuringEvent::new(on_start);
        emitter.pause = true;
        let emitter = Arc::new(emitter);
        let (h, _) = harness(
            source,
            FauxScript::new()
                .with_tool_call("write", serde_json::json!({"path":"b", "content":"b"})),
            Some(emitter.clone()),
            true,
        )
        .await;
        let pending = h.pending_resume().unwrap();
        let run = {
            let h = h.clone();
            tokio::spawn(async move { h.prompt_text("new prompt", vec![]).await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), emitter.ready.notified())
            .await
            .unwrap();
        assert!(h.resume_pending().await.is_err());
        assert_eq!(h.pending_resume(), Some(pending));
        h.abort().await.unwrap();
        emitter.proceed.notify_one();
        run.await.unwrap().unwrap();
        validate_history(&h).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_follow_up_can_be_cleared_after_run_end_and_stays_cancelled_on_restore() {
    let mut emitter = AppendDuringEvent::new(|event| matches!(event, AgentEvent::AgentEnd { .. }));
    emitter.pause = true;
    let emitter = Arc::new(emitter);
    let (h, _) = harness(
        fresh_session(),
        FauxScript::new().with_text("done"),
        Some(emitter.clone()),
        false,
    )
    .await;
    let run = {
        let h = h.clone();
        tokio::spawn(async move { h.prompt_text("answer", vec![]).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), emitter.ready.notified())
        .await
        .unwrap();
    let queued = h.follow_up(context_message()).await.unwrap();
    emitter.proceed.notify_one();
    run.await.unwrap().unwrap();
    assert_eq!(h.queued_messages().await.unwrap().follow_up.len(), 1);
    h.clear_queue().await.unwrap();
    validate_history(&h).await;
    let session = h.session().clone();
    drop(h);
    let (restored, _) = harness(
        session,
        FauxScript::new().with_text("next reply"),
        None,
        true,
    )
    .await;
    assert!(restored.queued_messages().await.unwrap().is_empty());
    restored.prompt_text("next turn", vec![]).await.unwrap();
    assert!(restored
        .get_entry(&queued.entry_id)
        .await
        .unwrap()
        .is_none());
    validate_history(&restored).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restored_legacy_run_bound_queues_can_be_cancelled_after_their_run_finished() {
    use rpi_harness::agent_harness::CancelQueuedOutcome;
    use rpi_harness::session::types::{
        provisioned_into_entry, LaneRecord, OperationFinishedRecord, OperationIntent,
        OperationOutcome, OperationStartedRecord, ProvisionedEntry, ProvisionedKind,
        QueueEnqueuedRecord, QueueKind, RecordBase,
    };
    for kind in [QueueKind::Steer, QueueKind::FollowUp, QueueKind::NextRun] {
        for clear_all in [false, true] {
            if kind == QueueKind::NextRun && clear_all {
                continue;
            }
            let session = fresh_session();
            let base = |id: &str| RecordBase {
                id: id.into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 0,
            };
            session
                .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
                    base: base("legacy-run"),
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
            let mut target = provisioned_into_entry(
                ProvisionedEntry {
                    id: "legacy-queued".into(),
                    kind: ProvisionedKind::Message {
                        message: context_message(),
                        terminate: None,
                    },
                },
                0,
                None,
                0,
            )
            .to_flat_json();
            let object = target.as_object_mut().unwrap();
            for key in ["seq", "parentId", "timestamp"] {
                object.remove(key);
            }
            session
                .append_record(LaneRecord::QueueEnqueued(QueueEnqueuedRecord {
                    base: base("legacy-enqueue"),
                    queue: kind,
                    run_id: Some("legacy-run".into()),
                    target,
                }))
                .await
                .unwrap();
            session
                .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
                    base: base("legacy-finished"),
                    run_id: "legacy-run".into(),
                    outcome: OperationOutcome::Completed,
                    error: None,
                    result: None,
                }))
                .await
                .unwrap();
            let (h, _) = harness(session.clone(), FauxScript::new(), None, true).await;
            if clear_all {
                let removed = h.clear_queue().await.unwrap();
                assert_eq!(removed.steering.len() + removed.follow_up.len(), 1);
            } else {
                assert_eq!(
                    h.cancel_queued("legacy-queued").await.unwrap().outcome,
                    CancelQueuedOutcome::Cancelled
                );
            }
            validate_history(&h).await;
            let records = session.find_records(&RecordQuery::default()).await.unwrap();
            assert!(records.iter().any(|record| matches!(record, LaneRecord::QueueCancelled(cancel) if cancel.entry_id == "legacy-queued" && cancel.run_id.as_deref() == Some("legacy-run"))));
            drop(h);
            let (restored, _) = harness(
                session,
                FauxScript::new().with_text("next reply"),
                None,
                true,
            )
            .await;
            assert!(restored.queued_messages().await.unwrap().is_empty());
            restored.prompt_text("next turn", vec![]).await.unwrap();
            assert!(restored.get_entry("legacy-queued").await.unwrap().is_none());
            validate_history(&restored).await;
        }
    }
}
