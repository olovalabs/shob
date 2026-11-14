//! Real JSONL close/reopen -> AgentHarness::create recovery; no process kill or provider I/O.
use rpi_agent::{AgentError, AgentMessage, AgentTool, AgentToolResult, ToolResultPartial};
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_ai::{
    frames::AssistantMessageFrameEncoder,
    types::{Api, AssistantMessage, AssistantMessageEvent, Content, StopReason, Tool, UserMessage},
};
use rpi_harness::agent_harness::AgentHarness;
use rpi_harness::session::{
    jsonl::{JsonlSessionCreateOptions, JsonlSessionRepo, JsonlSessionRepoOptions},
    memory::SystemClock,
    session::{DefaultIdGenerator, Session},
    types::{
        AssistantFrameOp, AssistantFrameRecord, BranchBounds, Entry, EntryOrder, EntryQuery,
        LaneRecord, OperationIntent, OperationStartedRecord, ProvisionedEntry, ProvisionedKind,
        RecordBase, ToolReplay, ToolStartedRecord,
    },
};
use rpi_harness::types::{AgentHarnessOptions, HarnessTool, ToolReplay as HarnessReplay};
use rpi_tools::{FileSystem, OsExecutionEnv};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio_util::sync::CancellationToken;

struct Counter {
    schema: Tool,
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl AgentTool for Counter {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "Counter"
    }
    async fn execute(
        &self,
        _: &str,
        _: Value,
        _: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(AgentToolResult::text("local-only"))
    }
}
fn recording() -> Value {
    json!({"version":1,"isError":false,"result":{"content":[{"type":"text","text":"upstream-only"}],"details":{"review":183}}})
}
fn base(id: &str) -> RecordBase {
    RecordBase {
        id: id.into(),
        seq: 0,
        lane: "main".into(),
        timestamp: 0,
    }
}
fn call(mark: Option<Value>) -> Content {
    let mut c = json!({"type":"toolCall","id":"c1","name":"counter","arguments":{"command":"counter-only"}});
    if let Some(mark) = mark {
        c["recorded"] = mark;
    }
    serde_json::from_value(c).unwrap()
}
async fn reopen_case(
    mark: Option<Value>,
    committed: bool,
    ended: bool,
    replay_record: bool,
) -> (usize, bool, String) {
    let tmp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(tmp.path().to_path_buf()));
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs,
        sessions_root: tmp.path().join("sessions").to_string_lossy().into_owned(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });
    let storage = Arc::new(
        repo.create_typed(&JsonlSessionCreateOptions {
            id: Some("review183".into()),
            parent_session_id: None,
            cwd: tmp.path().to_string_lossy().into_owned(),
            metadata: None,
        })
        .await
        .unwrap(),
    );
    let metadata = storage.jsonl_metadata();
    let session = Session::new(storage.clone(), None);
    session
        .append_message(AgentMessage::User(UserMessage::new("fixture", 0)))
        .await
        .unwrap();
    session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base("run"),
            source_leaf_id: session.get_leaf_id().await.unwrap(),
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        }))
        .await
        .unwrap();
    let mut assistant = AssistantMessage::empty(Api::Faux, "faux", "faux", 0);
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content.push(call(mark.clone()));
    if committed {
        session
            .append_entry(
                ProvisionedEntry {
                    id: "assistant".into(),
                    kind: ProvisionedKind::Message {
                        message: AgentMessage::Assistant(Box::new(assistant)),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
    } else {
        let mut started = assistant.clone();
        started.content = vec![call(None)];
        let mut events = vec![
            AssistantMessageEvent::Start {
                partial: Arc::new(AssistantMessage::empty(Api::Faux, "faux", "faux", 0)),
            },
            AssistantMessageEvent::ToolCallStart {
                content_index: 0,
                partial: Arc::new(started),
            },
        ];
        if ended {
            let Content::ToolCall(c) = &assistant.content[0] else {
                unreachable!()
            };
            events.push(AssistantMessageEvent::ToolCallEnd {
                content_index: 0,
                tool_call: c.clone(),
                partial: Arc::new(assistant),
            });
        }
        let mut encoder = AssistantMessageFrameEncoder::new();
        for (i, event) in events.iter().enumerate() {
            let frame = encoder.encode(event).unwrap().unwrap();
            session
                .append_record(LaneRecord::AssistantFrame(AssistantFrameRecord {
                    base: base(&format!("frame-{i}")),
                    run_id: "run".into(),
                    stream_index: 0,
                    op: AssistantFrameOp::Append,
                    frame: Some(serde_json::to_value(frame).unwrap()),
                }))
                .await
                .unwrap();
        }
        // A tool-start record must refer to the actual reserved assistant,
        // including when its binding reached disk before the entry itself.
        session
            .append_record(LaneRecord::AssistantFrame(AssistantFrameRecord {
                base: base("binding"),
                run_id: "run".into(),
                stream_index: 0,
                op: AssistantFrameOp::CommitStream,
                frame: Some(json!({"entryId":"assistant"})),
            }))
            .await
            .unwrap();
    }
    if replay_record {
        session
            .append_record(LaneRecord::ToolStarted(ToolStartedRecord {
                base: base("started"),
                run_id: "run".into(),
                assistant_entry_id: "assistant".into(),
                tool_index: 0,
                tool_call_id: "c1".into(),
                tool_name: "counter".into(),
                effective_args: json!({"command":"counter-only"}),
                result_entry_id: "result".into(),
                replay: ToolReplay::Safe,
            }))
            .await
            .unwrap();
    }
    drop(session);
    drop(storage);
    let reopened = repo.open_by_jsonl_metadata(&metadata).await.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let tool = Counter {
        calls: calls.clone(),
        schema: Tool {
            name: "counter".into(),
            description: "Counter only".into(),
            parameters: rpi_ai::types::Schema::new(json!({"type":"object"})),
            constrained_sampling: None,
        },
    };
    let provider = FauxProvider::new(FauxScript::new().with_text("unused"));
    let h = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        tools: vec![HarnessTool::new(Arc::new(tool)).with_replay(HarnessReplay::Safe)],
        session: Session::new(Arc::new(reopened), None),
        allow_existing_session: true,
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(h
        .session()
        .find_open_operations("main", Some(1))
        .await
        .unwrap()
        .is_empty());
    let entries = h
        .session()
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .await
        .unwrap();
    let results: Vec<_> = entries
        .iter()
        .filter_map(|e| match e {
            Entry::Message(m) => match &m.message {
                AgentMessage::ToolResult(t) => Some(t),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1, "one recovered tool result: {entries:?}");
    let text: String = results[0]
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    let result = (calls.load(Ordering::SeqCst), results[0].is_error, text);
    eprintln!("committed={committed}, ended={ended}, replay-record={replay_record}, mark={mark:?} => {result:?}; pending_resume={:?}", h.pending_resume());
    // Also reload the recovered result from disk, not merely the live reducer.
    drop(h);
    let reopened = repo.open_by_jsonl_metadata(&metadata).await.unwrap();
    let entries2 = Session::new(Arc::new(reopened), None)
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
        )
        .await
        .unwrap();
    // Existing optional details serde maps explicit null to None on reload;
    // normalize that legacy equivalence, including the ordinary-call control.
    let mut expected = entries;
    for entry in &mut expected {
        if let Entry::Message(m) = entry {
            if let AgentMessage::ToolResult(r) = &mut m.message {
                if r.details == Some(Value::Null) {
                    r.details = None;
                }
            }
        }
    }
    assert_eq!(entries2, expected);
    result
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review183_jsonl_reopen_retains_recorded_outcomes_without_replay() {
    for committed in [false, true] {
        assert_eq!(
            reopen_case(Some(recording()), committed, true, true).await,
            (0, false, "upstream-only".into())
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review183_jsonl_invalid_recordings_fail_closed() {
    for committed in [false, true] {
        for mark in [Value::Null, json!({"version":1}), json!({"version":99})] {
            let result = reopen_case(Some(mark), committed, true, true).await;
            assert_eq!(result.0, 0);
            assert!(result.1);
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review183_jsonl_ordinary_safe_replay_control() {
    for committed in [false, true] {
        let result = reopen_case(None, committed, true, true).await;
        assert_eq!(result.0, 1);
        assert!(!result.1);
        assert!(result.2.contains("local-only"));
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review183_jsonl_prefix_without_end_does_not_execute() {
    let result = reopen_case(None, false, false, false).await;
    assert_eq!(result.0, 0);
    assert!(result.1);
}
