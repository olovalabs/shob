//! Recovery must preserve durable identity and stop entering tools after abort.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use rpi_agent::{AgentError, AgentMessage, AgentTool, AgentToolResult, ToolResultPartial};
use rpi_ai::providers::faux::{faux_assistant_message, faux_tool_call, FauxProvider, FauxScript};
use rpi_ai::types::{AssistantMessage, StopReason, ToolResultMessage, ToolResultRole};
use rpi_harness::agent_harness::{AgentHarness, AgentLane};
use rpi_harness::frame_progress::unknown_tool_outcomes;
use rpi_harness::runtime::SessionRuntime;
use rpi_harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
use rpi_harness::session::types::*;
use rpi_harness::session::{DefaultIdGenerator, Session, SqliteSessionRepo, SystemClock};
use rpi_harness::types::{AgentHarnessOptions, HarnessTool};
use rpi_tools::{FileSystem, OsExecutionEnv};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn base(id: &str) -> RecordBase {
    RecordBase {
        id: id.into(),
        seq: 0,
        lane: "main".into(),
        timestamp: 0,
    }
}
fn assistant(calls: usize) -> AssistantMessage {
    let mut assistant = faux_assistant_message("", StopReason::ToolUse);
    assistant.content = (0..calls)
        .map(|index| {
            let mut call = faux_tool_call("effect", serde_json::json!({"index":index}));
            if let rpi_ai::types::Content::ToolCall(call) = &mut call {
                call.id = format!("call-{index}");
            }
            call
        })
        .collect();
    assistant
}
fn result(index: usize) -> AgentMessage {
    AgentMessage::ToolResult(Box::new(ToolResultMessage {
        role: ToolResultRole,
        tool_call_id: format!("call-{index}"),
        tool_name: "effect".into(),
        content: vec![rpi_ai::types::Content::text("external result")],
        details: None,
        usage: None,
        added_tool_names: vec![],
        is_error: false,
        timestamp: 0,
    }))
}
async fn append(session: &Session, id: &str, message: AgentMessage) {
    session
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
async fn setup_interrupted(session: &Session, calls: usize, replay: Option<ToolReplay>) {
    session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base("old-run"),
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
    append(
        session,
        "assistant",
        AgentMessage::Assistant(Box::new(assistant(calls))),
    )
    .await;
    if let Some(replay) = replay {
        for index in 0..calls {
            session
                .append_record(LaneRecord::ToolStarted(ToolStartedRecord {
                    base: base(&format!("started-{index}")),
                    run_id: "old-run".into(),
                    assistant_entry_id: "assistant".into(),
                    tool_index: index as u32,
                    tool_call_id: format!("call-{index}"),
                    tool_name: "effect".into(),
                    effective_args: serde_json::json!({"index":index}),
                    result_entry_id: format!("reserved-{index}"),
                    replay,
                }))
                .await
                .unwrap();
        }
    }
}

struct Backend {
    _temp: tempfile::TempDir,
    kind: &'static str,
    path: String,
    memory: Option<Arc<dyn SessionStorage>>,
    metadata: SessionMetadata,
}
impl Backend {
    async fn create(kind: &'static str) -> (Self, Session) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session").to_string_lossy().into_owned();
        let storage: Arc<dyn SessionStorage> = match kind {
            "jsonl" => Arc::new(
                JsonlSessionStorage::create(
                    Arc::new(OsExecutionEnv::with_cwd(temp.path().into())) as Arc<dyn FileSystem>,
                    &path,
                    JsonlV4Header::new("review-contract".into(), 1, "/".into(), None, None, None),
                    Arc::new(SystemClock),
                    Arc::new(DefaultIdGenerator::new()),
                )
                .await
                .unwrap(),
            ),
            "sqlite" => Arc::new(
                SqliteSessionRepo::open(&path)
                    .unwrap()
                    .create(&SessionCreateOptions {
                        id: Some("review-contract".into()),
                        ..Default::default()
                    })
                    .await
                    .unwrap(),
            ),
            _ => AgentHarnessOptions::default().session.storage(),
        };
        let metadata = storage.metadata();
        let memory = (kind == "memory").then(|| storage.clone());
        (
            Self {
                _temp: temp,
                kind,
                path,
                memory,
                metadata,
            },
            Session::new(storage, None),
        )
    }
    async fn reopen(&self) -> Session {
        let storage: Arc<dyn SessionStorage> = match self.kind {
            "jsonl" => Arc::new(
                JsonlSessionStorage::load(
                    Arc::new(OsExecutionEnv::with_cwd(self._temp.path().into())),
                    &self.path,
                    Arc::new(SystemClock),
                    Arc::new(DefaultIdGenerator::new()),
                )
                .await
                .unwrap(),
            ),
            "sqlite" => Arc::new(
                SqliteSessionRepo::open(&self.path)
                    .unwrap()
                    .open(&self.metadata)
                    .await
                    .unwrap(),
            ),
            _ => self.memory.clone().unwrap(),
        };
        Session::new(storage, None)
    }
}
struct ReplayProbe {
    schema: rpi_ai::types::Tool,
    seen: Mutex<Vec<(String, bool)>>,
    pause: bool,
    fail_first: bool,
    entered: Notify,
    release: Notify,
}
impl ReplayProbe {
    fn new(pause: bool, fail_first: bool) -> Arc<Self> {
        Arc::new(Self {
            schema: rpi_ai::types::Tool {
                name: "effect".into(),
                description: "review recovery probe".into(),
                parameters: rpi_ai::types::Schema::new(serde_json::json!({"type":"object"})),
                constrained_sampling: None,
            },
            seen: Mutex::new(vec![]),
            pause,
            fail_first,
            entered: Notify::new(),
            release: Notify::new(),
        })
    }
    fn tools(self: &Arc<Self>) -> Vec<HarnessTool> {
        vec![HarnessTool::new(self.clone()).with_replay(rpi_harness::types::ToolReplay::Safe)]
    }
}
#[async_trait::async_trait]
impl AgentTool for ReplayProbe {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "recovery side effect probe"
    }
    async fn execute(
        &self,
        id: &str,
        _: serde_json::Value,
        signal: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let first = {
            let mut seen = self.seen.lock().unwrap();
            let first = seen.is_empty();
            seen.push((id.into(), signal.is_cancelled()));
            first
        };
        if first && self.pause {
            self.entered.notify_one();
            self.release.notified().await;
        }
        if first && self.fail_first {
            return Err(AgentError::tool("the real replay failed"));
        }
        let mut result = AgentToolResult::text("the real replay output");
        result.details = serde_json::json!({"execution":id});
        Ok(result)
    }
}
async fn terminals(session: &Session) -> Vec<LaneRecord> {
    session
        .find_records(&RecordQuery {
            record_type: Some("operation_finished"),
            ..Default::default()
        })
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn conflicting_result_identity_keeps_recovery_open_without_replaying_any_call() {
    for kind in ["memory", "sqlite", "jsonl"] {
        for policy in [ToolReplay::Never, ToolReplay::Safe] {
            let (backend, session) = Backend::create(kind).await;
            setup_interrupted(&session, 2, Some(policy)).await;
            // Conflict on the SECOND call: validate the whole batch before
            // entering the first, even when both replay policies allow it.
            append(&session, "foreign-result", result(1)).await;
            let foreign = session.get_entry("foreign-result").await.unwrap();
            drop(session);
            let probe = ReplayProbe::new(false, false);
            for _ in 0..2 {
                let session = backend.reopen().await;
                let restored = AgentHarness::create(AgentHarnessOptions {
                    session: session.clone(),
                    allow_existing_session: true,
                    tools: probe.tools(),
                    ..Default::default()
                })
                .await;
                assert!(
                    restored.is_err(),
                    "{kind}: a foreign entry must not prove the reserved result landed"
                );
                assert!(
                    unknown_tool_outcomes(&session, "main", Some("assistant"), &assistant(2))
                        .await
                        .is_err()
                );
                assert!(terminals(&session).await.is_empty());
                assert_eq!(
                    session
                        .find_open_operations("main", None)
                        .await
                        .unwrap()
                        .len(),
                    1
                );
                for index in 0..2 {
                    assert!(session
                        .get_entry(&format!("reserved-{index}"))
                        .await
                        .unwrap()
                        .is_none());
                }
                assert_eq!(session.get_entry("foreign-result").await.unwrap(), foreign);
                assert!(
                    probe.seen.lock().unwrap().is_empty(),
                    "no tool may replay before conflict validation"
                );
                assert_eq!(
                    SessionRuntime::new(session)
                        .recover_lane("main")
                        .await
                        .unwrap()
                        .tool_frames
                        .len(),
                    2
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exact_reserved_results_stay_settled_across_two_restores() {
    for kind in ["memory", "sqlite", "jsonl"] {
        let (backend, session) = Backend::create(kind).await;
        setup_interrupted(&session, 2, Some(ToolReplay::Safe)).await;
        for index in 0..2 {
            append(&session, &format!("reserved-{index}"), result(index)).await;
        }
        drop(session);
        let probe = ReplayProbe::new(false, false);
        for _ in 0..2 {
            let session = backend.reopen().await;
            let restored = AgentHarness::create(AgentHarnessOptions {
                session: session.clone(),
                allow_existing_session: true,
                tools: probe.tools(),
                ..Default::default()
            })
            .await
            .unwrap();
            assert_eq!(terminals(&session).await.len(), 1);
            assert!(SessionRuntime::new(session)
                .recover_lane("main")
                .await
                .unwrap()
                .is_clean());
            assert!(probe.seen.lock().unwrap().is_empty());
            drop(restored);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_without_a_started_record_keeps_its_existing_landed_result() {
    let session = AgentHarnessOptions::default().session;
    setup_interrupted(&session, 1, None).await;
    append(&session, "existing", result(0)).await;
    let original = session.get_entry("existing").await.unwrap();
    let probe = ReplayProbe::new(false, false);
    let restored = AgentHarness::create(AgentHarnessOptions {
        session: session.clone(),
        allow_existing_session: true,
        tools: probe.tools(),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(session.get_entry("existing").await.unwrap(), original);
    assert_eq!(
        session
            .find_entries(&EntryQuery::default())
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(terminals(&session).await.len(), 1);
    assert!(probe.seen.lock().unwrap().is_empty());
    drop(restored);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_joins_the_active_replay_and_preserves_remaining_calls_for_reopen() {
    for calls in [1, 2] {
        for fail_first in [false, true] {
            let (backend, session) = Backend::create("jsonl").await;
            let probe = ReplayProbe::new(true, fail_first);
            let provider =
                FauxProvider::new(FauxScript::new().with_text("must not request more work"));
            let harness = AgentHarness::create(AgentHarnessOptions {
                session: session.clone(),
                model: provider.default_model().clone(),
                models: vec![provider.clone()],
                tools: probe.tools(),
                ..Default::default()
            })
            .await
            .unwrap();
            setup_interrupted(&session, calls, Some(ToolReplay::Safe)).await;
            let run_ends = Arc::new(AtomicUsize::new(0));
            let ended = run_ends.clone();
            let _listener = harness
                .events()
                .on::<rpi_harness::events::RunEndEvent, _>(move |_| {
                    ended.fetch_add(1, Ordering::SeqCst);
                });
            let active = harness.clone();
            let mut run =
                tokio::spawn(async move { active.prompt_text("next prompt", vec![]).await });
            tokio::time::timeout(std::time::Duration::from_secs(5), probe.entered.notified())
                .await
                .unwrap();
            harness.abort().await.unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(10), &mut run)
                    .await
                    .is_err(),
                "must join the executing replay"
            );
            probe.release.notify_one();
            let result = tokio::time::timeout(std::time::Duration::from_secs(5), run)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                *probe.seen.lock().unwrap(),
                vec![("call-0".into(), false)],
                "cancelled recovery must not enter another tool"
            );
            assert!(
                matches!(&result, Err(rpi_harness::result::HarnessError::Agent { message }) if message.contains("cancelled")),
                "cancelled recovery must not start a new run: {result:?}"
            );
            assert!(
                harness.take_write_failures().is_empty(),
                "cancellation is not a storage failure"
            );
            assert_eq!(provider.state().call_count.load(Ordering::SeqCst), 0);
            assert_eq!(run_ends.load(Ordering::SeqCst), 0);
            assert!(terminals(&session).await.is_empty());
            assert_eq!(
                session
                    .find_open_operations("main", None)
                    .await
                    .unwrap()
                    .len(),
                1
            );
            let first = session.get_entry("reserved-0").await.unwrap().unwrap();
            assert!(
                matches!(&first,Entry::Message(entry) if matches!(&entry.message,
                AgentMessage::ToolResult(result) if result.is_error==fail_first && result.content.iter().any(|content|
                    matches!(content,rpi_ai::types::Content::Text(text) if text.text.contains(if fail_first {"the real replay failed"} else {"the real replay output"})))))
            );
            if calls == 2 {
                assert!(session.get_entry("reserved-1").await.unwrap().is_none());
            }
            drop(_listener);
            drop(harness);
            drop(session);
            for _ in 0..2 {
                let session = backend.reopen().await;
                let restored = AgentHarness::create(AgentHarnessOptions {
                    session: session.clone(),
                    allow_existing_session: true,
                    tools: probe.tools(),
                    ..Default::default()
                })
                .await
                .unwrap();
                assert_eq!(
                    session.get_entry("reserved-0").await.unwrap(),
                    Some(first.clone()),
                    "never replay or overwrite the joined call"
                );
                assert_eq!(probe.seen.lock().unwrap().len(), calls);
                assert!(probe
                    .seen
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(_, cancelled)| !cancelled));
                assert_eq!(terminals(&session).await.len(), 1);
                assert_eq!(
                    session
                        .find_entries(&EntryQuery::default())
                        .await
                        .unwrap()
                        .len(),
                    calls + 1
                );
                assert!(SessionRuntime::new(session)
                    .recover_lane("main")
                    .await
                    .unwrap()
                    .is_clean());
                drop(restored);
            }
        }
    }
}
