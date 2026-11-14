use super::*;
use rpi_ai::providers::faux::{faux_assistant_message, FauxProvider, FauxScript, FauxStep};
use rpi_ai::types::StopReason;
use rpi_harness::agent_harness::AgentLane;
use rpi_harness::types::AgentHarnessOptions;

async fn parent(script: FauxScript) -> AgentHarness {
    let provider = FauxProvider::new(script);
    AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        session: memory_session("parent", "root"),
        ..Default::default()
    })
    .await
    .unwrap()
}
fn sink() -> SideQuestionSink {
    Arc::new(|_| Ok(json!({"ok":true})))
}
async fn command(manager: &SideQuestions, parent: &AgentHarness, args: Value) -> Value {
    manager.handle(parent, 7, args, sink()).await.unwrap()
}
async fn idle(manager: &SideQuestions) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while manager.0.capacity.available_permits() != 8 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("worker must release actual execution capacity");
}
async fn done(manager: &SideQuestions, parent: &AgentHarness) -> Value {
    idle(manager).await;
    command(manager, parent, json!({"op":"status"})).await
}
async fn running(manager: &SideQuestions) {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if manager.views().iter().any(|v| v["status"] == "running") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn independent_tool_free_question_streams_and_leaves_parent_unchanged() {
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::text("main answer"),
        FauxStep::factory(|ctx| {
            assert!(ctx.tools.is_empty());
            let encoded = serde_json::to_string(ctx).unwrap();
            assert!(encoded.contains("committed context"));
            assert!(encoded.contains("为什么"));
            faux_assistant_message("# Markdown\n\n**独立回答**", StopReason::Stop)
        }),
    ]);
    let parent = parent(script).await;
    parent
        .prompt_text("committed context", vec![])
        .await
        .unwrap();
    let before = parent.get_tip_id().await.unwrap();
    let manager = SideQuestions::default();
    command(&manager, &parent, json!({"op":"start","question":"为什么"})).await;
    let result = done(&manager, &parent).await;
    assert_eq!(result["status"], "done");
    assert!(result["text"].as_str().unwrap().contains("**独立回答**"));
    assert_eq!(parent.get_tip_id().await.unwrap(), before);
    assert_eq!(manager.views()[0]["turns"].as_array().unwrap().len(), 1);
    assert_eq!(
        manager
            .handle(&parent, 8, json!({"op":"status"}), sink())
            .await
            .unwrap()["status"],
        "idle"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn follow_up_has_real_roles_and_successful_side_history_only() {
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::text("unique first answer"),
        FauxStep::factory(|ctx| {
            let encoded = serde_json::to_string(&ctx.messages).unwrap();
            assert!(encoded.contains("unique first answer"));
            assert!(encoded.contains("first question"));
            assert!(encoded.contains("follow up"));
            assert!(ctx
                .messages
                .iter()
                .any(|m| matches!(m, rpi_ai::Message::Assistant(_))));
            assert!(ctx.tools.is_empty());
            faux_assistant_message("second answer", StopReason::Stop)
        }),
    ]);
    let parent = parent(script).await;
    let manager = SideQuestions::default();
    let first = command(
        &manager,
        &parent,
        json!({"op":"start","question":"first question"}),
    )
    .await;
    done(&manager, &parent).await;
    let second = command(
        &manager,
        &parent,
        json!({"op":"start","question":"follow up"}),
    )
    .await;
    let result = done(&manager, &parent).await;
    assert_eq!(first["threadId"], second["threadId"]);
    assert_eq!(result["text"], "second answer");
    assert_eq!(result["turns"].as_array().unwrap().len(), 2);
    assert!(parent.get_tip_id().await.unwrap().is_none());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_is_fifo_and_cancel_clears_pending_without_touching_main() {
    let parent = parent(
        FauxScript::new()
            .with_text("slow answer")
            .with_tokens_per_second(0.1),
    )
    .await;
    let manager = SideQuestions::default();
    command(&manager, &parent, json!({"op":"start","question":"first"})).await;
    running(&manager).await;
    for i in 0..8 {
        let result = command(
            &manager,
            &parent,
            json!({"op":"start","question":format!("queued-{i}")}),
        )
        .await;
        assert_eq!(result["status"], "queued");
    }
    assert!(manager
        .handle(
            &parent,
            7,
            json!({"op":"start","question":"overflow"}),
            sink()
        )
        .await
        .unwrap_err()
        .contains("queue is full"));
    let queued = command(&manager, &parent, json!({"op":"status"})).await;
    assert_eq!(queued["queued"][0], "queued-0");
    assert_eq!(queued["queued"][7], "queued-7");
    assert_eq!(
        command(&manager, &parent, json!({"op":"hide"})).await["visible"],
        false
    );
    assert_eq!(
        command(&manager, &parent, json!({"op":"show"})).await["visible"],
        true
    );
    command(&manager, &parent, json!({"op":"cancel"})).await;
    let cancelled = done(&manager, &parent).await;
    assert_eq!(cancelled["status"], "cancelled");
    assert!(cancelled["queued"].as_array().unwrap().is_empty());
    assert!(parent.get_tip_id().await.unwrap().is_none());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_requests_execute_in_order_and_inherit_predecessor_answers() {
    let script = FauxScript::new().with_tokens_per_second(30.0);
    script.set_responses(vec![
        FauxStep::text("first answer has several words to wait for"),
        FauxStep::factory(|ctx| {
            let text = serde_json::to_string(ctx).unwrap();
            assert!(text.contains("first answer has several words"));
            assert!(text.contains("second question"));
            faux_assistant_message("second answer", StopReason::Stop)
        }),
        FauxStep::factory(|ctx| {
            let text = serde_json::to_string(ctx).unwrap();
            assert!(text.contains("second answer"));
            assert!(text.contains("third question"));
            faux_assistant_message("third answer", StopReason::Stop)
        }),
    ]);
    let parent = parent(script).await;
    let manager = SideQuestions::default();
    command(
        &manager,
        &parent,
        json!({"op":"start","question":"first question"}),
    )
    .await;
    running(&manager).await;
    assert_eq!(
        command(
            &manager,
            &parent,
            json!({"op":"start","question":"second question"})
        )
        .await["status"],
        "queued"
    );
    command(
        &manager,
        &parent,
        json!({"op":"start","question":"third question"}),
    )
    .await;
    let result = done(&manager, &parent).await;
    assert_eq!(result["text"], "third answer");
    let turns = result["turns"].as_array().unwrap();
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[1]["question"], "second question");
    assert!(parent.get_tip_id().await.unwrap().is_none());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn show_empty_new_clear_and_stale_ui_are_thread_scoped() {
    let parent = parent(FauxScript::new().with_text("answer")).await;
    let manager = SideQuestions::default();
    let empty = command(&manager, &parent, json!({"op":"show"})).await;
    assert_eq!(empty["status"], "idle");
    let thread_id = empty["threadId"].as_str().unwrap().to_owned();
    manager
        .ui_handle(
            &parent,
            7,
            thread_id.clone(),
            json!({"op":"start","question":"q"}),
        )
        .await
        .unwrap();
    done(&manager, &parent).await;
    let new = command(
        &manager,
        &parent,
        json!({"op":"new","question":"new topic"}),
    )
    .await;
    assert_ne!(new["threadId"], thread_id);
    assert!(manager
        .ui_handle(&parent, 7, thread_id, json!({"op":"cancel"}))
        .await
        .is_err());
    let result = done(&manager, &parent).await;
    assert_eq!(result["turns"].as_array().unwrap().len(), 1);
    command(&manager, &parent, json!({"op":"clear"})).await;
    assert!(manager.views().is_empty());
    assert_eq!(
        command(&manager, &parent, json!({"op":"status"})).await["status"],
        "idle"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completed_running_and_empty_views_retire_on_parent_session_switch() {
    for mode in ["empty", "done", "running"] {
        let parent = parent(if mode == "running" {
            FauxScript::new()
                .with_text("slow")
                .with_tokens_per_second(0.1)
        } else {
            FauxScript::new().with_text("answer")
        })
        .await;
        let manager = SideQuestions::default();
        if mode == "empty" {
            command(&manager, &parent, json!({"op":"show"})).await;
        } else {
            command(&manager, &parent, json!({"op":"start","question":"q"})).await;
        }
        if mode == "done" {
            done(&manager, &parent).await;
        }
        assert_eq!(manager.views().len(), 1);
        let next = memory_session("next", "root");
        next.view("main")
            .append_message(
                UserMessage {
                    role: UserRole,
                    content: "next".into(),
                    timestamp: 0,
                }
                .into(),
            )
            .await
            .unwrap();
        parent.set_session(next).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !manager.views().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(manager.0.registry.lock().unwrap().threads.is_empty());
        idle(&manager).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_retirement_revokes_ui_and_cleans_running_queue() {
    let parent = parent(
        FauxScript::new()
            .with_text("slow")
            .with_tokens_per_second(0.1),
    )
    .await;
    let manager = SideQuestions::default();
    command(&manager, &parent, json!({"op":"start","question":"q"})).await;
    command(&manager, &parent, json!({"op":"start","question":"queued"})).await;
    manager.cancel_owner(7);
    assert!(manager.views().is_empty());
    idle(&manager).await;
    assert!(manager
        .handle(&parent, 7, json!({"op":"show"}), sink())
        .await
        .unwrap_err()
        .contains("unloaded"));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn long_markdown_is_not_a_preview_and_history_is_bounded() {
    let text = format!(
        "# Heading\n\n{}\nLAST-LINE",
        "中文 **answer**\n".repeat(120)
    );
    let script = FauxScript::new();
    script.set_responses((0..18).map(|_| FauxStep::text(text.clone())).collect());
    let parent = parent(script).await;
    let manager = SideQuestions::default();
    for _ in 0..18 {
        command(&manager, &parent, json!({"op":"start","question":"q"})).await;
        let result = done(&manager, &parent).await;
        assert_eq!(result["text"], text);
    }
    let result = command(&manager, &parent, json!({"op":"status"})).await;
    assert_eq!(result["turns"].as_array().unwrap().len(), 16);
    assert_eq!(result["historyTruncated"], true);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn side_question_runs_while_main_provider_is_still_streaming() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let started = Arc::new(AtomicBool::new(false));
    let writer = started.clone();
    let script = FauxScript::new().with_tokens_per_second(15.0);
    script.set_responses(vec![
        FauxStep::factory(move |_| {
            writer.store(true, Ordering::Release);
            faux_assistant_message(
                "main task is still streaming with several words",
                StopReason::Stop,
            )
        }),
        FauxStep::text("side answer"),
    ]);
    let parent = parent(script).await;
    let lane = parent.lane("main");
    let main = tokio::spawn(async move { lane.prompt_text("main task", vec![]).await.unwrap() });
    while !started.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let before = parent.get_tip_id().await.unwrap();
    let manager = SideQuestions::default();
    command(&manager, &parent, json!({"op":"start","question":"side?"})).await;
    assert_eq!(done(&manager, &parent).await["text"], "side answer");
    assert!(!main.is_finished());
    assert_eq!(parent.get_tip_id().await.unwrap(), before);
    main.await.unwrap();
    assert_eq!(
        command(&manager, &parent, json!({"op":"status"})).await["status"],
        "done"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_and_output_limits_are_terminal() {
    let parent = parent(
        FauxScript::new()
            .with_text("slow")
            .with_tokens_per_second(0.1),
    )
    .await;
    let revision = Arc::new(AtomicU64::new(0));
    let timed = Arc::new(Job::new("timeout".into(), "q".into(), revision));
    timed.state.lock().unwrap().status = "running";
    let (child, _) = parent
        .create_side_question(
            memory_session("timeout", "parent"),
            Arc::new(Emitter(timed.clone())),
        )
        .await
        .unwrap();
    run(child, timed.clone(), "q".into(), Duration::ZERO).await;
    assert_eq!(timed.snapshot()["status"], "failed");
    assert!(timed.snapshot()["error"]
        .as_str()
        .unwrap()
        .contains("timed out"));
    timed.state.lock().unwrap().status = "running";
    let message = Arc::new(faux_assistant_message("", StopReason::Stop));
    timed.observe(AgentEvent::MessageUpdate {
        message_id: None,
        message: message.clone(),
        assistant_message_event: AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "x".repeat(MAX_OUTPUT + 1),
            partial: message,
        },
    });
    assert!(timed.snapshot()["error"]
        .as_str()
        .unwrap()
        .contains("output limit"));
    assert_eq!(timed.snapshot()["text"], "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_partial_responses_are_visible_but_not_replayed_as_successful_context() {
    let script = FauxScript::new();
    script.set_responses(vec![
        FauxStep::message(faux_assistant_message(
            "unique failed partial",
            StopReason::Error,
        )),
        FauxStep::factory(|ctx| {
            let text = serde_json::to_string(&ctx.messages).unwrap();
            assert!(!text.contains("unique failed partial"));
            assert!(!text.contains("failed question"));
            assert!(text.contains("successful retry"));
            faux_assistant_message("successful answer", StopReason::Stop)
        }),
    ]);
    let parent = parent(script).await;
    let manager = SideQuestions::default();
    command(
        &manager,
        &parent,
        json!({"op":"start","question":"failed question"}),
    )
    .await;
    assert_eq!(done(&manager, &parent).await["status"], "failed");
    command(
        &manager,
        &parent,
        json!({"op":"start","question":"successful retry"}),
    )
    .await;
    let result = done(&manager, &parent).await;
    assert_eq!(result["text"], "successful answer");
    assert_eq!(result["turns"][0]["status"], "failed");
    assert_eq!(result["turns"][1]["status"], "done");
}

#[test]
fn history_limits_also_bound_failed_and_cancelled_turns() {
    for status in ["done", "failed", "cancelled"] {
        let revision = Arc::new(AtomicU64::new(0));
        let mut state = ThreadState {
            turns: vec![],
            pending: VecDeque::new(),
            active: None,
            worker: false,
            visible: true,
            focus_revision: 0,
            history_truncated: false,
            epoch: 0,
        };
        for n in 0..20 {
            let job = Arc::new(Job::new(n.to_string(), n.to_string(), revision.clone()));
            job.state.lock().unwrap().status = status;
            state.turns.push(job);
        }
        Thread::trim(&mut state);
        assert_eq!(state.turns.len(), 16);
        assert_eq!(state.turns[0].question, "4");
        assert!(state.history_truncated);
        for job in &state.turns {
            job.state.lock().unwrap().text = "x".repeat(MAX_OUTPUT);
        }
        Thread::trim(&mut state);
        let bytes: usize = state
            .turns
            .iter()
            .map(|job| job.question.len() + job.state.lock().unwrap().text.len())
            .sum();
        assert!(bytes <= MAX_HISTORY);
        assert_eq!(state.turns.len(), 3);
    }
}
