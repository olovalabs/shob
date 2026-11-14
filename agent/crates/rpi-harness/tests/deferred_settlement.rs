//! Deferred polls obey the same terminal ACK, lease and RunEnd protocol as runs.
use rpi_agent::AgentMessage;
use rpi_ai::provider::Provider;
use rpi_ai::providers::faux::{faux_assistant_message, DeferredPoll, FauxProvider, FauxScript};
use rpi_ai::types::{DeferredHandle, StopReason};
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_harness::events::{RunEndEvent, RunEndOutcome};
use rpi_harness::session::types::*;
use rpi_harness::types::AgentHarnessOptions;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

async fn assert_query_matches(h: &AgentHarness, result: &rpi_harness::agent_harness::RunResult) {
    let read = h.get_result(&result.run_id).await.unwrap().unwrap();
    assert_eq!(read.run_id, result.run_id);
    assert_eq!(
        format!("{:?}", read.outcome),
        format!("{:?}", result.outcome)
    );
}

async fn harness(provider: Arc<FauxProvider>) -> AgentHarness {
    AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap()
}
async fn park(h: &AgentHarness, p: &FauxProvider, with_operation: bool) -> DeferredHandle {
    let handle = DeferredHandle {
        provider: p.id().into(),
        model_id: p.default_model().id.clone(),
        api: p.default_model().api.as_str().into(),
        id: "deferred-review".into(),
        expires_at: None,
        poll_after_ms: None,
        data: None,
    };
    if with_operation {
        h.session()
            .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
                base: RecordBase {
                    id: "parked-run".into(),
                    seq: 0,
                    lane: "main".into(),
                    timestamp: 0,
                },
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
    }
    let mut parked = faux_assistant_message("", StopReason::Deferred);
    parked.deferred = Some(handle.clone());
    h.session()
        .view("main")
        .append_message(AgentMessage::Assistant(Box::new(parked)))
        .await
        .unwrap();
    handle
}
fn watch(
    h: &AgentHarness,
) -> (
    Arc<Mutex<Vec<RunEndEvent>>>,
    rpi_harness::events::OnUnsubscribe,
) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let out = events.clone();
    let subscription = h
        .events()
        .on::<RunEndEvent, _>(move |event| out.lock().unwrap().push(event.clone()));
    (events, subscription)
}
async fn finished(h: &AgentHarness) -> Vec<LaneRecord> {
    h.session()
        .find_records(&RecordQuery {
            record_type: Some("operation_finished"),
            ..Default::default()
        })
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ordinary_completion_emits_one_authoritative_run_end() {
    let h = harness(FauxProvider::new(FauxScript::new().with_text("done"))).await;
    let (events, _sub) = watch(&h);
    let r = h.prompt_text("finish", vec![]).await.unwrap();
    assert!(matches!(r.outcome, HarnessRunOutcome::Completed { .. }));
    assert_eq!(events.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deferred_completion_must_emit_the_authoritative_run_end_used_by_agent_settled() {
    let p = FauxProvider::new(FauxScript::new());
    p.push_deferred_poll(DeferredPoll::Message(faux_assistant_message(
        "ready",
        StopReason::Stop,
    )));
    let h = harness(p.clone()).await;
    let handle = park(&h, &p, true).await;
    let (events, _sub) = watch(&h);
    let r = h.resume(&handle.id).await.unwrap();
    assert!(matches!(r.outcome, HarnessRunOutcome::Completed { .. }));
    assert_eq!(r.run_id, "parked-run");
    assert_query_matches(&h, &r).await;
    assert_eq!(finished(&h).await.len(), 1);
    {
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].run_id, r.run_id);
        assert_eq!(events[0].outcome, RunEndOutcome::Completed);
        if let HarnessRunOutcome::Completed { leaf_id, .. } = &r.outcome {
            assert_eq!(&events[0].leaf_id, leaf_id);
        }
    }
    // A consumed historical handle cannot manufacture another settlement.
    assert!(h.resume(&handle.id).await.is_err());
    h.drive(&handle.id).await.unwrap();
    assert_eq!(events.lock().unwrap().len(), 1);
    assert_eq!(finished(&h).await.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn suspended_poll_keeps_its_identity_and_emits_no_terminal_event() {
    let p = FauxProvider::new(FauxScript::new());
    let h = harness(p.clone()).await;
    let handle = park(&h, &p, true).await;
    let next = DeferredHandle {
        id: "next-handle".into(),
        ..handle.clone()
    };
    p.push_deferred_poll(DeferredPoll::StillDeferred(next.clone()));
    p.push_deferred_poll(DeferredPoll::Message(faux_assistant_message(
        "ready",
        StopReason::Stop,
    )));
    let (events, _sub) = watch(&h);
    let r = h.resume(&handle.id).await.unwrap();
    assert_eq!(r.run_id, "parked-run");
    assert!(matches!(r.outcome, HarnessRunOutcome::Suspended { .. }));
    assert!(h.get_result(&r.run_id).await.unwrap().is_none());
    assert!(events.lock().unwrap().is_empty());
    assert!(finished(&h).await.is_empty());
    assert_eq!(
        h.session()
            .find_open_operations("main", None)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        h.resume(&handle.id).await.is_err(),
        "superseded handle is not current"
    );
    let r = h.resume(&next.id).await.unwrap();
    assert_eq!(r.run_id, "parked-run");
    assert_eq!(events.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deferred_error_and_abort_publish_the_correct_terminal_outcome() {
    for (reason, expected) in [
        (StopReason::Error, RunEndOutcome::Failed),
        (StopReason::Aborted, RunEndOutcome::Aborted),
    ] {
        let p = FauxProvider::new(FauxScript::new());
        p.push_deferred_poll(DeferredPoll::Message(faux_assistant_message(
            "terminal", reason,
        )));
        let h = harness(p.clone()).await;
        let handle = park(&h, &p, true).await;
        let (events, _sub) = watch(&h);
        let r = h.resume(&handle.id).await.unwrap();
        assert!(matches!(
            (&r.outcome, expected),
            (HarnessRunOutcome::Failed { .. }, RunEndOutcome::Failed)
                | (HarnessRunOutcome::Aborted { .. }, RunEndOutcome::Aborted)
        ));
        assert_eq!(events.lock().unwrap()[0].outcome, expected);
        assert_query_matches(&h, &r).await;
        let records = finished(&h).await;
        assert_eq!(records.len(), 1);
        let LaneRecord::OperationFinished(record) = &records[0] else {
            panic!("terminal record")
        };
        assert!(matches!(
            (record.outcome, expected),
            (OperationOutcome::Failed, RunEndOutcome::Failed)
                | (OperationOutcome::Aborted, RunEndOutcome::Aborted)
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn imported_deferred_message_establishes_terminal_proof_before_run_end() {
    let p = FauxProvider::new(FauxScript::new());
    p.push_deferred_poll(DeferredPoll::Message(faux_assistant_message(
        "ready",
        StopReason::Stop,
    )));
    let h = harness(p.clone()).await;
    let handle = park(&h, &p, false).await;
    let (events, _sub) = watch(&h);
    let r = h.resume(&handle.id).await.unwrap();
    assert!(!r.run_id.is_empty());
    assert_query_matches(&h, &r).await;
    assert_eq!(finished(&h).await.len(), 1);
    assert_eq!(events.lock().unwrap()[0].run_id, r.run_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn polled_tool_continuation_emits_one_run_end_after_its_result() {
    let p = FauxProvider::new(FauxScript::new().with_text("done"));
    let mut message = faux_assistant_message("", StopReason::ToolUse);
    message.content = vec![rpi_ai::types::Content::ToolCall(rpi_ai::types::ToolCall {
        kind: rpi_ai::types::ToolCallType,
        id: "polled-call".into(),
        name: "missing-tool".into(),
        arguments: serde_json::json!({}),
        thought_signature: None,
        namespace: None,
        recorded: None,
    })];
    p.push_deferred_poll(DeferredPoll::Message(message));
    let h = harness(p.clone()).await;
    let handle = park(&h, &p, true).await;
    let (events, _sub) = watch(&h);
    let r = h.resume(&handle.id).await.unwrap();
    assert!(matches!(r.outcome, HarnessRunOutcome::Completed { .. }));
    assert_ne!(r.run_id, "parked-run");
    assert_query_matches(&h, &r).await;
    assert_eq!(
        finished(&h).await.len(),
        2,
        "poll and continuation have separate native identities"
    );
    assert_eq!(
        events.lock().unwrap().len(),
        1,
        "only the logical continuation performs final plugin settlement"
    );
    assert_eq!(events.lock().unwrap()[0].run_id, r.run_id);
    assert!(h.session().find_entries(&EntryQuery::default()).await.unwrap().iter().any(|entry| matches!(entry, Entry::Message(entry) if matches!(&entry.message, AgentMessage::ToolResult(result) if result.tool_call_id == "polled-call"))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deferred_run_end_has_terminal_proof_and_holds_admission_until_observers_drain() {
    let p = FauxProvider::new(FauxScript::new());
    p.push_deferred_poll(DeferredPoll::Message(faux_assistant_message(
        "ready",
        StopReason::Stop,
    )));
    let h = harness(p.clone()).await;
    let handle = park(&h, &p, true).await;
    let received = Arc::new(Mutex::new(None));
    let out = received.clone();
    let observer = h.clone();
    let _sub = h.events().on::<RunEndEvent, _>(move |event| {
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                assert_eq!(
                    finished(&observer).await.len(),
                    1,
                    "terminal ACK precedes observer"
                );
                assert!(
                    observer.prompt_text("overlap", vec![]).await.is_err(),
                    "RunEnd must still own admission"
                );
                observer
                    .append_message(rpi_ai::UserMessage::new("observer context", 0).into())
                    .await
                    .unwrap()
            })
        });
        *out.lock().unwrap() = Some((event.clone(), result));
    });
    let r = h.resume(&handle.id).await.unwrap();
    let (event, accepted) = received.lock().unwrap().clone().unwrap();
    assert_eq!(event.run_id, r.run_id);
    assert!(
        h.session().get_entry(&accepted).await.unwrap().is_some(),
        "observer context is drained before return"
    );
    let HarnessRunOutcome::Completed { leaf_id, .. } = r.outcome else {
        panic!("completed")
    };
    assert_eq!(
        leaf_id, event.leaf_id,
        "RunResult retains the pre-observer terminal snapshot"
    );
    assert_ne!(h.get_leaf_id().await.unwrap().unwrap(), leaf_id);
    tokio::time::timeout(std::time::Duration::from_secs(1), h.wait_for_idle())
        .await
        .unwrap()
        .unwrap();
}

mod refusal {
    use super::*;
    use async_trait::async_trait;
    use rpi_tools::env::AppendFile;
    use rpi_tools::{FileContent, FileError, FileInfo, FileSystem};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;
    #[derive(Default)]
    struct Gate {
        armed: AtomicBool,
        truncate_fail: AtomicBool,
        fail_stage: AtomicBool,
        rename_mode: AtomicUsize,
        block_rename: AtomicBool,
        block_truncate: AtomicBool,
        entered: Notify,
        release: Notify,
    }
    struct GatedFs {
        inner: Arc<dyn FileSystem>,
        gate: Arc<Gate>,
    }
    struct GatedAppend {
        inner: Arc<dyn FileSystem>,
        path: String,
        gate: Arc<Gate>,
    }
    #[async_trait]
    impl AppendFile for GatedAppend {
        async fn append(&mut self, bytes: &[u8]) -> Result<(), FileError> {
            if self.gate.armed.load(Ordering::SeqCst)
                && String::from_utf8_lossy(bytes).contains("operation_finished")
            {
                return Err(FileError::new(
                    rpi_tools::FileErrorCode::Unknown,
                    "controlled terminal refusal",
                ));
            }
            self.inner
                .append_file(&self.path, FileContent::Bytes(bytes.to_vec()), None)
                .await
        }
    }
    include!("support/scoped_write_fs.rs");
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failed_terminal_write_keeps_operation_open_and_emits_no_run_end() {
        use rpi_harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
        use rpi_harness::session::{DefaultIdGenerator, Session, SystemClock};
        let gate = Arc::new(Gate::default());
        let fs: Arc<dyn FileSystem> = Arc::new(GatedFs {
            inner: Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into())),
            gate: gate.clone(),
        });
        let storage = JsonlSessionStorage::create(
            fs,
            "/session.jsonl",
            JsonlV4Header::new("failure".into(), 1, "/".into(), None, None, None),
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        let p = FauxProvider::new(FauxScript::new());
        p.push_deferred_poll(DeferredPoll::Message(faux_assistant_message(
            "ready",
            StopReason::Stop,
        )));
        let h = AgentHarness::create(AgentHarnessOptions {
            session: Session::new(Arc::new(storage), None),
            model: p.default_model().clone(),
            models: vec![p.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
        let handle = park(&h, &p, true).await;
        let (events, _sub) = watch(&h);
        gate.armed.store(true, Ordering::SeqCst);
        assert!(h.resume(&handle.id).await.is_err());
        assert!(events.lock().unwrap().is_empty());
        assert!(finished(&h).await.is_empty());
        assert_eq!(
            h.session()
                .find_open_operations("main", None)
                .await
                .unwrap()
                .len(),
            1
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), h.wait_for_idle())
            .await
            .unwrap()
            .unwrap();
    }
}
