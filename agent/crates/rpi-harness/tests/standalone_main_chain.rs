use async_trait::async_trait;
use rpi_agent::AgentMessage;
use rpi_ai::providers::faux::{faux_assistant_message, FauxProvider, FauxScript};
use rpi_ai::types::{Content, StopReason, ToolResultMessage, ToolResultRole};
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_harness::session::{
    jsonl::{JsonlSessionStorage, JsonlV4Header},
    DefaultIdGenerator, Session, SystemClock,
};
use rpi_harness::types::AgentHarnessOptions;
use rpi_tools::env::AppendFile;
use rpi_tools::{FileContent, FileError, FileInfo, FileSystem};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

async fn harness(session: Option<Session>) -> AgentHarness {
    let p = FauxProvider::new(
        FauxScript::new()
            .with_text("first answer")
            .with_text("second answer"),
    );
    let mut opts = AgentHarnessOptions {
        model: p.default_model().clone(),
        models: vec![p],
        allow_existing_session: true,
        ..Default::default()
    };
    if let Some(session) = session {
        opts.session = session;
    }
    AgentHarness::create(opts).await.unwrap()
}
fn completed(
    r: &rpi_harness::agent_harness::RunResult,
) -> (&str, &str, &rpi_ai::types::AssistantMessage) {
    match &r.outcome {
        HarnessRunOutcome::Completed {
            leaf_id,
            final_entry_id,
            final_message,
        } => (leaf_id, final_entry_id, final_message),
        x => panic!("{x:?}"),
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_get_result_immediately_after_completion() {
    let h = harness(None).await;
    let first = h.prompt_text("first", vec![]).await.unwrap();
    let read = h.get_result(&first.run_id).await.unwrap().unwrap();
    assert_eq!(completed(&read), completed(&first));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_result_of_first_run_survives_a_second_run() {
    let h = harness(None).await;
    let first = h.prompt_text("first", vec![]).await.unwrap();
    let second = h.prompt_text("second", vec![]).await.unwrap();
    let read = h.get_result(&first.run_id).await.unwrap().unwrap();
    eprintln!("first={first:?}\nsecond={second:?}\nread-first={read:?}");
    assert_eq!(
        completed(&read),
        completed(&first),
        "historical result must not become the next run's answer"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_result_must_be_scoped_to_the_requested_lane() {
    let h = harness(None).await;
    let first = h.prompt_text("main", vec![]).await.unwrap();
    h.session().create_lane("other", None).await.unwrap();
    assert!(
        h.lane("other")
            .get_result(&first.run_id)
            .await
            .unwrap()
            .is_none(),
        "other lane must not invent a terminal result from main"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_without_an_assistant_must_not_inherit_a_previous_answer() {
    let h = AgentHarness::create(Default::default()).await.unwrap();
    h.append_message(AgentMessage::Assistant(Box::new(faux_assistant_message(
        "historical answer",
        StopReason::Stop,
    ))))
    .await
    .unwrap();
    let result = h.prompt_text("no provider", vec![]).await.unwrap();
    let read = h.get_result(&result.run_id).await.unwrap().unwrap();
    eprintln!("returned={result:?}\nread={read:?}");
    assert!(matches!(
        read.outcome,
        HarnessRunOutcome::Failed {
            final_entry_id: None,
            final_message: None,
            ..
        }
    ));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inspect_execution_must_include_the_persisted_result() {
    let h = harness(None).await;
    let mut assistant = faux_assistant_message("", StopReason::ToolUse);
    assistant
        .content
        .push(Content::tool_call("call", "read", serde_json::json!({})));
    let call_entry = h
        .append_message(AgentMessage::Assistant(Box::new(assistant)))
        .await
        .unwrap();
    let result_entry = h
        .append_message(AgentMessage::ToolResult(Box::new(ToolResultMessage {
            role: ToolResultRole,
            tool_call_id: "call".into(),
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
    let info = h.inspect_execution("call").await.unwrap().unwrap();
    eprintln!("call={call_entry:?}; result={result_entry:?}; inspect={info:?}");
    assert_eq!(info.entry_id.as_deref(), Some(call_entry.as_str()));
    assert_eq!(info.result_entry_id.as_deref(), Some(result_entry.as_str()));
}

#[derive(Default)]
struct Gate {
    fail_cancel: AtomicBool,
    fail_started: AtomicBool,
    fail_second: AtomicBool,
    fail_terminal: AtomicBool,
    tear_cancel: AtomicBool,
    block_kind: AtomicUsize,
    block_entry: std::sync::Mutex<Option<String>>,
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
        let text = String::from_utf8_lossy(bytes);
        let kind = self.gate.block_kind.load(Ordering::SeqCst);
        let block_entry = {
            let mut target = self.gate.block_entry.lock().unwrap();
            if target.as_ref().is_some_and(|id| text.contains(id)) {
                target.take();
                true
            } else {
                false
            }
        };
        if (kind == 1 && text.contains("queue_enqueued"))
            || (kind == 2 && text.contains("queue_cancelled"))
            || (kind == 3 && text.contains("operation_started"))
            || block_entry
        {
            self.gate.block_kind.store(0, Ordering::SeqCst);
            self.gate.entered.notify_one();
            self.gate.release.notified().await;
        }
        if text.contains("queue_cancelled") && self.gate.tear_cancel.swap(false, Ordering::SeqCst) {
            let length = bytes.iter().position(|byte| *byte == b'\n').unwrap() + 1;
            self.inner
                .append_file(
                    &self.path,
                    FileContent::Bytes(bytes[..length].to_vec()),
                    None,
                )
                .await?;
            return Err(FileError::new(
                rpi_tools::FileErrorCode::Unknown,
                "partial cancellation batch",
            ));
        }
        if (text.contains("second-reserved")
            && !text.contains("operation_started")
            && self.gate.fail_second.swap(false, Ordering::SeqCst))
            || (text.contains("operation_finished")
                && self.gate.fail_terminal.swap(false, Ordering::SeqCst))
        {
            return Err(FileError::new(
                rpi_tools::FileErrorCode::Unknown,
                "deterministic refused entry/terminal",
            ));
        }
        if (text.contains("queue_cancelled") && self.gate.fail_cancel.swap(false, Ordering::SeqCst))
            || (text.contains("operation_started")
                && self.gate.fail_started.swap(false, Ordering::SeqCst))
        {
            return Err(FileError::new(
                rpi_tools::FileErrorCode::Unknown,
                "deterministic refused append",
            ));
        }
        self.inner
            .append_file(&self.path, FileContent::Bytes(bytes.to_vec()), None)
            .await
    }
}
include!("support/scoped_write_fs.rs");
include!("support/standalone_cases.rs");
include!("support/standalone_lifecycle.rs");

async fn disk() -> (
    tempfile::TempDir,
    String,
    Arc<Gate>,
    Arc<dyn FileSystem>,
    AgentHarness,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join("session.jsonl")
        .to_string_lossy()
        .replace('\\', "/");
    let gate = Arc::new(Gate::default());
    let fs: Arc<dyn FileSystem> = Arc::new(GatedFs {
        inner: Arc::new(rpi_tools::OsExecutionEnv::with_cwd(dir.path().into())),
        gate: gate.clone(),
    });
    let storage = JsonlSessionStorage::create(
        fs.clone(),
        &path,
        JsonlV4Header::new(
            "audit".into(),
            1,
            dir.path().display().to_string(),
            None,
            None,
            None,
        ),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let h = harness(Some(Session::new(Arc::new(storage), None))).await;
    (dir, path, gate, fs, h)
}
async fn reopen(path: &str, fs: Arc<dyn FileSystem>) -> AgentHarness {
    let storage = JsonlSessionStorage::load(
        fs,
        path,
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    harness(Some(Session::new(Arc::new(storage), None))).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_cancel_must_not_acknowledge_success_or_resurrect_after_reopen() {
    let (_dir, path, gate, fs, h) = disk().await;
    let queued = h
        .steer(rpi_ai::UserMessage::new("cancel me", 0).into())
        .await
        .unwrap();
    gate.fail_cancel.store(true, Ordering::SeqCst);
    let cancelled = h.cancel_queued(&queued.entry_id).await;
    let memory = h.queued_messages().await.unwrap();
    drop(h);
    let restored = reopen(&path, fs).await.queued_messages().await.unwrap();
    eprintln!("cancelled={cancelled:?}; memory={memory:?}; reopened={restored:?}");
    assert!(
        cancelled.is_err(),
        "refused durable cancellation must not return Cancelled"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_clear_must_not_return_a_successful_draft_restore() {
    let (_dir, path, gate, fs, h) = disk().await;
    h.steer(rpi_ai::UserMessage::new("restore me", 0).into())
        .await
        .unwrap();
    gate.fail_cancel.store(true, Ordering::SeqCst);
    let cleared = h.clear_queue().await;
    drop(h);
    let restored = reopen(&path, fs).await.queued_messages().await.unwrap();
    eprintln!("clear={cleared:?}; reopened={restored:?}");
    assert!(
        cleared.is_err(),
        "draft restore must not succeed while durable queue is unchanged"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_next_run_must_remain_deliverable_after_a_refused_first_prompt() {
    let (_dir, _path, gate, _fs, h) = disk().await;
    let queued = h
        .next_run(rpi_ai::UserMessage::new("accepted queued input", 0).into())
        .await
        .unwrap();
    gate.fail_started.store(true, Ordering::SeqCst);
    assert!(h.prompt_text("first attempt", vec![]).await.is_err());
    let result = h.prompt_text("retry", vec![]).await.unwrap();
    assert!(matches!(
        result.outcome,
        HarnessRunOutcome::Completed { .. }
    ));
    let accepted = h
        .session()
        .view("main")
        .get_entry(&queued.entry_id)
        .await
        .unwrap();
    let cancelled = h.cancel_queued(&queued.entry_id).await.unwrap();
    eprintln!("accepted entry after retry={accepted:?}; cancel={cancelled:?}");
    assert!(
        accepted.is_some(),
        "accepted durable input must not disappear until process restart"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_successful_cancel_does_not_resurrect() {
    let (_dir, path, _gate, fs, h) = disk().await;
    let q = h
        .steer(rpi_ai::UserMessage::new("cancel me", 0).into())
        .await
        .unwrap();
    h.cancel_queued(&q.entry_id).await.unwrap();
    drop(h);
    assert!(reopen(&path, fs)
        .await
        .queued_messages()
        .await
        .unwrap()
        .is_empty());
}
