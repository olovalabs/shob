// Regression contracts from the local review of PR #208. These use only the
// native host and deterministic emitter/storage gates, without extensions.

struct LifecycleTool {
    schema: rpi_ai::types::Tool,
    calls: AtomicUsize,
    running: AtomicBool,
    overlapped: AtomicBool,
    entered: Notify,
    release: Notify,
    dropped: Notify,
    first_signal: std::sync::Mutex<Option<CancellationToken>>,
}

impl LifecycleTool {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            schema: rpi_ai::types::Tool {
                name: "effect".into(),
                description: "lifecycle test".into(),
                parameters: rpi_ai::types::Schema::new(serde_json::json!({
                    "type": "object", "properties": {}
                })),
                constrained_sampling: None,
            },
            calls: AtomicUsize::new(0),
            running: AtomicBool::new(false),
            overlapped: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
            dropped: Notify::new(),
            first_signal: std::sync::Mutex::new(None),
        })
    }
}

struct LiveExecution<'a>(&'a LifecycleTool);
impl Drop for LiveExecution<'_> {
    fn drop(&mut self) {
        self.0.running.store(false, Ordering::SeqCst);
        self.0.dropped.notify_one();
    }
}

#[async_trait]
impl rpi_agent::AgentTool for LifecycleTool {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "lifecycle test"
    }
    async fn execute(
        &self,
        _: &str,
        _: serde_json::Value,
        signal: CancellationToken,
        _: Arc<dyn Fn(rpi_agent::ToolResultPartial) + Send + Sync>,
    ) -> Result<rpi_agent::AgentToolResult, rpi_agent::AgentError> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        if index == 0 {
            self.running.store(true, Ordering::SeqCst);
            let _live = LiveExecution(self);
            *self.first_signal.lock().unwrap() = Some(signal.clone());
            self.entered.notify_one();
            tokio::select! {
                _ = self.release.notified() => {},
                _ = signal.cancelled() => {},
            }
        } else if self.running.load(Ordering::SeqCst) {
            self.overlapped.store(true, Ordering::SeqCst);
        }
        Ok(rpi_agent::AgentToolResult::text("effect finished"))
    }
}

async fn lifecycle_harness(tool: Arc<LifecycleTool>) -> AgentHarness {
    let p = FauxProvider::new(
        FauxScript::new()
            .with_tool_call("effect", serde_json::json!({}))
            .with_text("native completion"),
    );
    AgentHarness::create(AgentHarnessOptions {
        model: p.default_model().clone(),
        models: vec![p],
        tools: vec![rpi_harness::types::HarnessTool::new(tool)
            .with_replay(rpi_harness::types::ToolReplay::Safe)],
        ..Default::default()
    })
    .await
    .unwrap()
}

async fn lifecycle_wait(notify: &Notify) {
    tokio::time::timeout(std::time::Duration::from_secs(5), notify.notified())
        .await
        .expect("deterministic lifecycle gate timed out");
}

fn lifecycle_result(call_id: &str) -> AgentMessage {
    AgentMessage::ToolResult(Box::new(ToolResultMessage {
        role: ToolResultRole,
        tool_call_id: call_id.into(),
        tool_name: "effect".into(),
        content: vec![Content::text("unrelated imported result")],
        details: None,
        usage: None,
        added_tool_names: vec![],
        is_error: false,
        timestamp: 0,
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_inherited_native_checkpoint_is_not_legacy_matching() {
    let tool = LifecycleTool::new();
    tool.release.notify_one();
    let h = lifecycle_harness(tool).await;
    let run = h.prompt_text("native execution", vec![]).await.unwrap();
    let records = h
        .session()
        .find_records(&RecordQuery {
            record_type: Some("tool_started"),
            run_id: Some(run.run_id),
            ..Default::default()
        })
        .await
        .unwrap();
    let LaneRecord::ToolStarted(checkpoint) = &records[0] else {
        panic!("native checkpoint")
    };
    h.session()
        .create_lane("fork", Some(&checkpoint.result_entry_id))
        .await
        .unwrap();
    let fork = h.lane("fork");
    let unrelated = fork
        .append_message(lifecycle_result(&checkpoint.tool_call_id))
        .await
        .unwrap();
    let info = fork
        .inspect_execution(&checkpoint.tool_call_id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(unrelated, checkpoint.result_entry_id);
    assert_eq!(
        info.result_entry_id.as_deref(),
        Some(checkpoint.result_entry_id.as_str())
    );

    // An inherited checkpoint does not make its off-branch result visible.
    h.session()
        .create_lane("without-result", Some(&checkpoint.assistant_entry_id))
        .await
        .unwrap();
    let other = h.lane("without-result");
    other
        .append_message(lifecycle_result(&checkpoint.tool_call_id))
        .await
        .unwrap();
    assert!(other
        .inspect_execution(&checkpoint.tool_call_id)
        .await
        .unwrap()
        .unwrap()
        .result_entry_id
        .is_none());

    // A newer imported call with a reused provider ID cannot borrow the old
    // native checkpoint either: assistant identity, not call ID, selects it.
    let mut assistant = faux_assistant_message("", StopReason::ToolUse);
    assistant.content.push(Content::tool_call(
        &checkpoint.tool_call_id,
        "effect",
        serde_json::json!({}),
    ));
    let imported_call = fork
        .append_message(AgentMessage::Assistant(Box::new(assistant)))
        .await
        .unwrap();
    let imported_result = fork
        .append_message(lifecycle_result(&checkpoint.tool_call_id))
        .await
        .unwrap();
    let info = fork
        .inspect_execution(&checkpoint.tool_call_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.entry_id.as_deref(), Some(imported_call.as_str()));
    assert_eq!(
        info.result_entry_id.as_deref(),
        Some(imported_result.as_str())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_root_drop_retires_live_execute_before_recovery() {
    let tool = LifecycleTool::new();
    let h = lifecycle_harness(tool.clone()).await;
    let caller = {
        let h = h.clone();
        tokio::spawn(async move { h.prompt_text("start effect", vec![]).await })
    };
    lifecycle_wait(&tool.entered).await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    lifecycle_wait(&tool.dropped).await;
    assert!(!tool.running.load(Ordering::SeqCst));
    assert!(tool
        .first_signal
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .is_cancelled());
    let second = h.prompt_text("retry", vec![]).await.unwrap();
    assert!(matches!(
        second.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    assert_eq!(tool.calls.load(Ordering::SeqCst), 2, "safe recovery ran");
    assert!(!tool.overlapped.load(Ordering::SeqCst));
}

struct HoldLifecycleInjection {
    entered: Arc<Notify>,
}
impl rpi_agent::AgentEmitter for HoldLifecycleInjection {
    fn emit(&self, event: rpi_agent::AgentEvent) -> futures::future::BoxFuture<'static, ()> {
        let entered = self.entered.clone();
        Box::pin(async move {
            if matches!(event, rpi_agent::AgentEvent::MessageStart { message: AgentMessage::User(user), .. }
                if user.content == rpi_ai::types::UserContent::Text("accepted-injection".into()))
            {
                entered.notify_one();
                std::future::pending::<()>().await;
            }
        })
    }
    fn try_emit(&self, _: rpi_agent::AgentEvent) {}
}

async fn lifecycle_queue_input(h: &AgentHarness, followup: bool) -> String {
    let message = rpi_ai::UserMessage::new("accepted-injection", 0).into();
    if followup {
        h.follow_up(message).await.unwrap().entry_id
    } else {
        h.steer(message).await.unwrap().entry_id
    }
}

async fn lifecycle_injection_drop(followup: bool, at_entry_ack: bool) {
    let (_dir, path, gate, fs, h) = disk().await;
    // Retrying a pending follow-up legitimately requires an extra provider turn.
    h.set_models(vec![FauxProvider::new(
        FauxScript::new()
            .with_text("first answer")
            .with_text("second answer")
            .with_text("follow-up answer"),
    )])
    .await
    .unwrap();
    let id = lifecycle_queue_input(&h, followup).await;
    let entered = Arc::new(Notify::new());
    if at_entry_ack {
        *gate.block_entry.lock().unwrap() = Some(id.clone());
    } else {
        h.set_agent_emitter(Some(Arc::new(HoldLifecycleInjection {
            entered: entered.clone(),
        })))
        .await
        .unwrap();
    }
    let caller = {
        let h = h.clone();
        tokio::spawn(async move { h.prompt_text("first attempt", vec![]).await })
    };
    lifecycle_wait(if at_entry_ack {
        &gate.entered
    } else {
        &entered
    })
    .await;
    assert!(h.get_entry(&id).await.unwrap().is_none());
    if !at_entry_ack {
        // In-flight reservations are neither displayed as pending nor cleared
        // out from underneath the accepted handoff.
        assert!(h.queued_messages().await.unwrap().is_empty());
        assert!(h.clear_queue().await.unwrap().is_empty());
    }
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    h.set_agent_emitter(None).await.unwrap();
    let second = {
        let h = h.clone();
        tokio::spawn(async move { h.prompt_text("retry same process", vec![]).await })
    };
    if at_entry_ack {
        assert!(!second.is_finished(), "owned entry ACK holds the gate");
        gate.release.notify_one();
    }
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(second.outcome, HarnessRunOutcome::Completed { .. }),
        "{second:?}"
    );
    assert!(h.get_entry(&id).await.unwrap().is_some());
    assert!(h.queued_messages().await.unwrap().is_empty());
    assert!(h
        .session()
        .find_records(&RecordQuery {
            target_entry_id: Some(id),
            pending_queue_only: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .is_empty());
    drop(h);
    assert!(reopen(&path, fs)
        .await
        .queued_messages()
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_steering_drop_before_commit_is_deliverable() {
    lifecycle_injection_drop(false, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_followup_drop_before_commit_is_deliverable() {
    lifecycle_injection_drop(true, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_steering_owned_ack_survives_caller_drop() {
    lifecycle_injection_drop(false, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_followup_owned_ack_survives_caller_drop() {
    lifecycle_injection_drop(true, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_dropped_handoff_keeps_only_the_uncommitted_prefix_pending() {
    let (_dir, path, _gate, fs, original) = disk().await;
    let session = original.session().clone();
    drop(original);
    let p = FauxProvider::new(
        FauxScript::new()
            .with_text("first answer")
            .with_text("retry answer"),
    );
    let h = AgentHarness::create(AgentHarnessOptions {
        model: p.default_model().clone(),
        models: vec![p],
        session,
        steering_mode: rpi_agent::QueueMode::All,
        allow_existing_session: true,
        ..Default::default()
    })
    .await
    .unwrap();
    let committed = h
        .steer(rpi_ai::UserMessage::new("committed-prefix", 0).into())
        .await
        .unwrap();
    let pending = lifecycle_queue_input(&h, false).await;
    let entered = Arc::new(Notify::new());
    h.set_agent_emitter(Some(Arc::new(HoldLifecycleInjection {
        entered: entered.clone(),
    })))
    .await
    .unwrap();
    let caller = {
        let h = h.clone();
        tokio::spawn(async move { h.prompt_text("first attempt", vec![]).await })
    };
    lifecycle_wait(&entered).await;
    assert!(h.get_entry(&committed.entry_id).await.unwrap().is_some());
    assert!(h.get_entry(&pending).await.unwrap().is_none());
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    h.set_agent_emitter(None).await.unwrap();
    let queued = h.queued_messages().await.unwrap();
    assert_eq!(queued.steering.len(), 1);
    assert_eq!(queued.steering[0].text, "accepted-injection");
    assert!(queued.steering[0].images.is_empty());
    let second = h.prompt_text("retry", vec![]).await.unwrap();
    assert!(
        matches!(second.outcome, HarnessRunOutcome::Completed { .. }),
        "{second:?}"
    );
    assert!(h.get_entry(&pending).await.unwrap().is_some());
    assert!(h.queued_messages().await.unwrap().is_empty());
    drop(h);
    assert!(reopen(&path, fs)
        .await
        .queued_messages()
        .await
        .unwrap()
        .is_empty());
}
