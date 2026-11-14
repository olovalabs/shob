//! Tool completion proofs must identify the invocation, not a provider ID alone.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use rpi_agent::{AgentError, AgentMessage, AgentTool, AgentToolResult, ToolResultPartial};
use rpi_ai::providers::faux::{faux_assistant_message, faux_tool_call};
use rpi_ai::types::{AssistantMessage, StopReason, ToolResultMessage, ToolResultRole};
use rpi_harness::agent_harness::AgentHarness;
use rpi_harness::frame_progress::unknown_tool_outcomes;
use rpi_harness::runtime::{RecoveryFinding, SessionRuntime};
use rpi_harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
use rpi_harness::session::memory::SystemClock;
use rpi_harness::session::types::*;
use rpi_harness::session::{DefaultIdGenerator, Session};
use rpi_harness::types::{AgentHarnessOptions, HarnessTool};
use rpi_tools::{FileSystem, OsExecutionEnv};
use tokio_util::sync::CancellationToken;

fn base(id: &str) -> RecordBase {
    RecordBase {
        id: id.into(),
        seq: 0,
        lane: "main".into(),
        timestamp: 0,
    }
}
fn assistant() -> AssistantMessage {
    let mut message = faux_assistant_message("", StopReason::ToolUse);
    let mut call = faux_tool_call("write", serde_json::json!({"path":"a.txt"}));
    if let rpi_ai::types::Content::ToolCall(call) = &mut call {
        call.id = "reused-call".into();
    }
    message.content = vec![call];
    message
}
fn result(call: &str, name: &str) -> AgentMessage {
    AgentMessage::ToolResult(Box::new(ToolResultMessage {
        role: ToolResultRole,
        tool_call_id: call.into(),
        tool_name: name.into(),
        content: vec![rpi_ai::types::Content::text("a result")],
        details: None,
        usage: None,
        added_tool_names: vec![],
        is_error: false,
        timestamp: 0,
    }))
}
async fn append(session: &Session, id: &str, lane: &str, message: AgentMessage) {
    session
        .append_entry(
            ProvisionedEntry {
                id: id.into(),
                kind: ProvisionedKind::Message {
                    message,
                    terminate: None,
                },
            },
            lane,
        )
        .await
        .unwrap();
}
async fn start(session: &Session) {
    session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base("run"),
            source_leaf_id: session.view("main").get_leaf_id().await.unwrap(),
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
        "main",
        AgentMessage::Assistant(Box::new(assistant())),
    )
    .await;
    session
        .append_record(LaneRecord::ToolStarted(ToolStartedRecord {
            base: base("started"),
            run_id: "run".into(),
            assistant_entry_id: "assistant".into(),
            tool_index: 0,
            tool_call_id: "reused-call".into(),
            tool_name: "write".into(),
            effective_args: serde_json::json!({"path":"a.txt"}),
            result_entry_id: "reserved-result".into(),
            replay: ToolReplay::Never,
        }))
        .await
        .unwrap();
}
async fn finish(session: &Session) {
    session
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            base: base("finished"),
            run_id: "run".into(),
            outcome: OperationOutcome::Completed,
            error: None,
            result: None,
        }))
        .await
        .unwrap();
}

#[tokio::test]
async fn runtime_requires_the_reserved_result_not_a_reused_provider_call_id() {
    for origin in ["older-batch", "other-lane", "newer-batch", "same-batch"] {
        for closed in [false, true] {
            let session = AgentHarnessOptions::default().session;
            if origin == "older-batch" {
                append(
                    &session,
                    "older-assistant",
                    "main",
                    AgentMessage::Assistant(Box::new(assistant())),
                )
                .await;
                append(
                    &session,
                    "unrelated-result",
                    "main",
                    result("reused-call", "write"),
                )
                .await;
            }
            start(&session).await;
            match origin {
                "other-lane" => {
                    session
                        .storage()
                        .create_lane("side", Some("assistant"))
                        .await
                        .unwrap();
                    append(
                        &session,
                        "unrelated-result",
                        "side",
                        result("reused-call", "write"),
                    )
                    .await;
                }
                "newer-batch" => {
                    append(
                        &session,
                        "newer-assistant",
                        "main",
                        AgentMessage::Assistant(Box::new(assistant())),
                    )
                    .await;
                    append(
                        &session,
                        "unrelated-result",
                        "main",
                        result("reused-call", "write"),
                    )
                    .await;
                }
                "same-batch" => {
                    append(
                        &session,
                        "unrelated-result",
                        "main",
                        result("reused-call", "write"),
                    )
                    .await
                }
                _ => {}
            }
            if closed {
                finish(&session).await;
            }
            let runtime = SessionRuntime::new(session.clone());
            let report = runtime.recover_lane("main").await.unwrap();
            assert_eq!(
                report.tool_frames.len(),
                1,
                "{origin}, closed={closed}: unrelated result hid an unsettled invocation"
            );
            assert_eq!(report.tool_frames[0].result_entry_id, "reserved-result");
            assert_eq!(report.findings.iter().any(|finding| matches!(finding,
                RecoveryFinding::DanglingToolFrame { run_id, tool_call_id } if run_id=="run" && tool_call_id=="reused-call")),closed);
            session
                .storage()
                .move_lane("main", Some("assistant"))
                .await
                .unwrap();
            append(
                &session,
                "reserved-result",
                "main",
                result("reused-call", "write"),
            )
            .await;
            let report = runtime.recover_lane("main").await.unwrap();
            assert!(
                report.tool_frames.is_empty(),
                "the exact reserved result settles the invocation"
            );
            assert!(!report
                .findings
                .iter()
                .any(|f| matches!(f, RecoveryFinding::DanglingToolFrame { .. })));
            // Reports include historical lane records: navigating away from an
            // already completed invocation must not make it dangling again.
            session
                .storage()
                .move_lane("main", Some("assistant"))
                .await
                .unwrap();
            assert!(runtime
                .recover_lane("main")
                .await
                .unwrap()
                .tool_frames
                .is_empty());
        }
    }
}

#[tokio::test]
async fn runtime_reserved_entry_must_match_the_result_role_call_and_tool() {
    for mismatch in ["name", "call", "role", "none"] {
        let session = AgentHarnessOptions::default().session;
        start(&session).await;
        let message = match mismatch {
            "name" => result("reused-call", "external"),
            "call" => result("another-call", "write"),
            "role" => AgentMessage::User(rpi_ai::types::UserMessage::new("not a result", 0)),
            _ => result("reused-call", "write"),
        };
        append(&session, "reserved-result", "main", message).await;
        finish(&session).await;
        let report = SessionRuntime::new(session)
            .recover_lane("main")
            .await
            .unwrap();
        assert_eq!(
            report.tool_frames.len(),
            usize::from(mismatch != "none"),
            "{mismatch} mismatch is not completion proof"
        );
    }
}

struct ReplayProbe {
    schema: rpi_ai::types::Tool,
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl AgentTool for ReplayProbe {
    fn schema(&self) -> &rpi_ai::types::Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "must not replay"
    }
    async fn execute(
        &self,
        _: &str,
        _: serde_json::Value,
        _: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(AgentToolResult::text("unexpected execution"))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jsonl_recovery_requires_the_matching_tool_name_before_closing_the_operation() {
    let tmp = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(tmp.path().into()));
    let path = tmp
        .path()
        .join("identity.jsonl")
        .to_string_lossy()
        .into_owned();
    let inner = JsonlSessionStorage::create(
        fs.clone(),
        &path,
        JsonlV4Header::new("identity".into(), 1, "/".into(), None, None, None),
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let session = Session::new(Arc::new(inner), None);
    start(&session).await;
    append(
        &session,
        "external-result",
        "main",
        result("reused-call", "external"),
    )
    .await;
    drop(session);
    let probe = Arc::new(ReplayProbe {
        schema: rpi_ai::types::Tool {
            name: "write".into(),
            description: "replay identity probe".into(),
            parameters: rpi_ai::types::Schema::new(serde_json::json!({"type":"object"})),
            constrained_sampling: None,
        },
        calls: AtomicUsize::new(0),
    });
    for reopen in 0..2 {
        let storage = Arc::new(
            JsonlSessionStorage::load(
                fs.clone(),
                &path,
                Arc::new(SystemClock),
                Arc::new(DefaultIdGenerator::new()),
            )
            .await
            .unwrap(),
        );
        let session = Session::new(storage, None);
        let unresolved = unknown_tool_outcomes(&session, "main", Some("assistant"), &assistant())
            .await
            .unwrap();
        let restored = AgentHarness::create(AgentHarnessOptions {
            session: session.clone(),
            allow_existing_session: true,
            tools: vec![
                HarnessTool::new(probe.clone()).with_replay(rpi_harness::types::ToolReplay::Safe)
            ],
            ..Default::default()
        })
        .await
        .unwrap();
        let reserved = session.get_entry("reserved-result").await.unwrap();
        assert!(
            matches!(&reserved,Some(Entry::Message(entry)) if matches!(&entry.message,
            AgentMessage::ToolResult(result) if result.tool_call_id=="reused-call" && result.tool_name=="write" && result.is_error)),
            "the wrong-name result cannot hide the required synthetic result: {reserved:?}"
        );
        assert_eq!(unresolved.len(), usize::from(reopen == 0));
        assert!(session
            .find_open_operations("main", None)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            session
                .find_records(&RecordQuery {
                    record_type: Some("operation_finished"),
                    ..Default::default()
                })
                .await
                .unwrap()
                .len(),
            1
        );
        let entries = session
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            entries.len(),
            3,
            "one assistant, external result, correct result; no duplication on reopen"
        );
        let messages = entries
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Message(entry) => Some(entry.message),
                _ => None,
            })
            .collect();
        let projected = rpi_harness::messages::convert_to_llm(messages);
        let results: Vec<_> = projected
            .into_iter()
            .filter_map(|message| match message {
                rpi_ai::types::Message::ToolResult(result) => Some(result),
                _ => None,
            })
            .collect();
        let Some(Entry::Message(entry)) = reserved else {
            unreachable!()
        };
        let AgentMessage::ToolResult(expected) = entry.message else {
            unreachable!()
        };
        assert_eq!(
            results,
            vec![expected],
            "provider context must retain the actual recovered result, not the external lookalike"
        );
        assert!(SessionRuntime::new(session)
            .recover_lane("main")
            .await
            .unwrap()
            .is_clean());
        assert_eq!(
            probe.calls.load(Ordering::SeqCst),
            0,
            "original Never policy still prevents replay despite current Safe"
        );
        drop(restored);
    }
}

#[test]
fn provider_context_rejects_wrong_names_on_fast_and_repair_paths() {
    let mut observed = Vec::new();
    for has_correct_result in [false, true] {
        let mut messages = vec![
            AgentMessage::Assistant(Box::new(assistant())),
            result("reused-call", "external"),
        ];
        if has_correct_result {
            messages.push(result("reused-call", "write"));
        }
        let (repaired, report) = rpi_harness::messages::repair_orphaned_tool_calls(messages);
        let again = rpi_harness::messages::repair_orphaned_tool_calls(repaired.clone());
        assert_eq!(again.0, repaired, "repair must be idempotent");
        assert!(again.1.is_empty());
        let converted = rpi_harness::messages::convert_to_llm(repaired);
        let results: Vec<_> = converted
            .into_iter()
            .filter_map(|message| match message {
                rpi_ai::types::Message::ToolResult(result) => {
                    Some((result.tool_name.clone(), result.is_error))
                }
                _ => None,
            })
            .collect();
        observed.push((results, report.repaired_call_ids.len()));
    }
    assert_eq!(
        observed,
        vec![
            (vec![("write".to_string(), true)], 1),
            (vec![("write".to_string(), false)], 0),
        ],
        "neither the fast path nor the slow repair may consume a wrong-name result"
    );
}
