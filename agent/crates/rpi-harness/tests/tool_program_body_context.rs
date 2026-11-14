//! Reserved bodies are archival history, not semantic hook input. Cold digest
//! probes prove exclusion before decoding, not constant metadata/RSS or zero I/O
//! (small gaps may coalesce). Explicit history reads intentionally remain broad.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use rpi_agent::{AgentError, AgentMessage, AgentTool, AgentToolResult, ToolResultPartial};
use rpi_ai::providers::faux::{faux_assistant_message, faux_tool_call, FauxProvider, FauxScript};
use rpi_ai::types::StopReason;
use rpi_harness::{
    agent_harness::{AgentHarness, AgentLane},
    session::{
        context::{build_context_entries, build_session_context, SessionContextBuildOptions},
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        types::*,
        DefaultIdGenerator, InMemorySessionStorage, Session, SqliteSessionRepo,
    },
    tool_program_body::{CHUNK_CUSTOM_TYPE, COMMIT_CUSTOM_TYPE},
    types::{AgentHarnessOptions, HarnessTool},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

struct FixedClock;
impl rpi_harness::session::memory::Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        1
    }
}

fn memory() -> Session {
    Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "body-context".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FixedClock),
            Arc::new(DefaultIdGenerator::new()),
        )),
        None,
    )
}
async fn append(session: &Session, id: &str, kind: ProvisionedKind) {
    session
        .append_entry(
            ProvisionedEntry {
                id: id.into(),
                kind,
            },
            "main",
        )
        .await
        .unwrap();
}
async fn user(session: &Session, id: &str) {
    append(
        session,
        id,
        ProvisionedKind::Message {
            message: rpi_ai::UserMessage::new(id, 1).into(),
            terminate: None,
        },
    )
    .await;
}
async fn custom(session: &Session, id: &str, kind: &str, large: bool) {
    append(
        session,
        id,
        ProvisionedKind::Custom {
            custom_type: kind.into(),
            data: Some(json!({"payload": if large { "x".repeat(48 * 1024) } else { id.into() }})),
        },
    )
    .await;
}
async fn archives(session: &Session) {
    // Genuine reserved custom types; no writer/observer installation needed.
    for i in 0..48 {
        custom(session, &format!("archive-{i:03}"), CHUNK_CUSTOM_TYPE, true).await;
    }
    custom(session, "archive-commit", COMMIT_CUSTOM_TYPE, true).await;
}
fn semantic_ids(entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| {
            if let Entry::Custom(custom) = entry {
                assert_ne!(custom.custom_type, CHUNK_CUSTOM_TYPE);
                assert_ne!(custom.custom_type, COMMIT_CUSTOM_TYPE);
            }
            entry.id().to_owned()
        })
        .collect()
}
fn hooks(
    expected: Vec<String>,
    transforms: Arc<AtomicUsize>,
    projects: Arc<AtomicUsize>,
) -> SessionContextBuildOptions {
    let expected_projector = expected.clone();
    let mut options = SessionContextBuildOptions::default();
    options.entry_transforms.push(Arc::new(move |entries| {
        assert_eq!(semantic_ids(entries), expected);
        transforms.fetch_add(1, Ordering::SeqCst);
        entries.to_vec()
    }));
    options.entry_projectors.insert(
        "extension".into(),
        Arc::new(move |entry, index, entries| {
            assert_eq!(semantic_ids(entries), expected_projector);
            assert_eq!(entries[index].id(), entry.base.id);
            projects.fetch_add(1, Ordering::SeqCst);
            vec![rpi_ai::UserMessage::new(format!("projected:{}:{index}", entry.base.id), 2).into()]
        }),
    );
    for kind in [CHUNK_CUSTOM_TYPE, COMMIT_CUSTOM_TYPE] {
        options.entry_projectors.insert(
            kind.into(),
            Arc::new(|_, _, _| panic!("archival projector called")),
        );
    }
    options
}

#[tokio::test]
async fn default_context_and_explicit_history_agree_across_backends() {
    let temp = tempfile::tempdir().unwrap();
    let jsonl_path = temp
        .path()
        .join("context.jsonl")
        .to_string_lossy()
        .into_owned();
    let jsonl = JsonlSessionStorage::create(
        Arc::new(rpi_tools::OsExecutionEnv::with_cwd(temp.path().into())),
        &jsonl_path,
        JsonlV4Header::new("body-context".into(), 1, "/".into(), None, None, None),
        Arc::new(FixedClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    let sqlite = SqliteSessionRepo::with_clock_ids(
        temp.path().join("context.db"),
        Arc::new(FixedClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .unwrap()
    .create(&SessionCreateOptions {
        id: Some("body-context".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    let sessions = [
        memory(),
        Session::new(Arc::new(jsonl), None),
        Session::new(Arc::new(sqlite), None),
    ];
    let mut baseline = None;
    for session in sessions {
        append(
            &session,
            "model",
            ProvisionedKind::ModelChange {
                provider: "test".into(),
                model_id: "model".into(),
            },
        )
        .await;
        user(&session, "before").await;
        custom(&session, "old-body", CHUNK_CUSTOM_TYPE, false).await;
        append(
            &session,
            "cut",
            ProvisionedKind::Compaction {
                summary: "summary".into(),
                retained_tail: vec![],
                tokens_before: 42,
                details: None,
                usage: None,
            },
        )
        .await;
        user(&session, "after").await;
        custom(&session, "normal", "extension", false).await;
        custom(&session, "chunk", CHUNK_CUSTOM_TYPE, false).await;
        custom(&session, "commit", COMMIT_CUSTOM_TYPE, false).await;
        custom(
            &session,
            "not-reserved",
            "rpi.tool_program.body_chunk.v1.extra",
            false,
        )
        .await;
        let all = session
            .find_entries(&EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(all.len(), 9);
        let expected = build_session_context(&all, &Default::default());
        let actual = session
            .storage()
            .context_on_branch(
                &BranchBounds::default(),
                &session.view("main").get_leaf_id().await.unwrap().unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(actual.messages, expected.messages);
        assert_eq!(actual.model, expected.model);
        assert_eq!(actual.thinking_level, expected.thinking_level);
        assert_eq!(actual.active_tool_names, expected.active_tool_names);
        let shape = (
            actual.messages,
            actual.model,
            actual.thinking_level,
            actual.active_tool_names,
        );
        if let Some(baseline) = &baseline {
            assert_eq!(&shape, baseline);
        } else {
            baseline = Some(shape);
        }
        let filtered = session
            .view("main")
            .find_entries_on_branch(
                &EntryQuery {
                    exclude_tool_program_bodies: true,
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            semantic_ids(&filtered),
            ["model", "before", "cut", "after", "normal", "not-reserved"]
        );
        for kind in [CHUNK_CUSTOM_TYPE, COMMIT_CUSTOM_TYPE] {
            assert!(!session
                .find_entries(&EntryQuery {
                    custom_type: Some(kind.into()),
                    ..Default::default()
                })
                .await
                .unwrap()
                .is_empty());
            assert!(session
                .find_entries(&EntryQuery {
                    custom_type: Some(kind.into()),
                    exclude_tool_program_bodies: true,
                    ..Default::default()
                })
                .await
                .unwrap()
                .is_empty());
        }
        let transforms = Arc::new(AtomicUsize::new(0));
        let projects = Arc::new(AtomicUsize::new(0));
        let options = hooks(
            vec![
                "cut".into(),
                "after".into(),
                "normal".into(),
                "not-reserved".into(),
            ],
            transforms.clone(),
            projects.clone(),
        );
        let context = build_session_context(&all, &options);
        assert_eq!(context.messages.len(), 3);
        assert_eq!(transforms.load(Ordering::SeqCst), 1);
        assert_eq!(projects.load(Ordering::SeqCst), 1);
        assert_eq!(build_context_entries(&all, &Default::default()).len(), 4);
    }
}

async fn cold(session_path: &str, dir: &std::path::Path) -> Session {
    let store = JsonlSessionStorage::load(
        Arc::new(rpi_tools::OsExecutionEnv::with_cwd(dir.into())),
        session_path,
        Arc::new(FixedClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap();
    Session::new(Arc::new(store), None)
}
async fn disk(path: &str, dir: &std::path::Path) -> Session {
    Session::new(
        Arc::new(
            JsonlSessionStorage::create(
                Arc::new(rpi_tools::OsExecutionEnv::with_cwd(dir.into())),
                path,
                JsonlV4Header::new("cold-context".into(), 1, "/".into(), None, None, None),
                Arc::new(FixedClock),
                Arc::new(DefaultIdGenerator::new()),
            )
            .await
            .unwrap(),
        ),
        None,
    )
}
fn poison_archival_payloads(path: &str) {
    let raw = std::fs::read_to_string(path).unwrap();
    let changed = raw
        .lines()
        .map(|line| {
            if line.contains(CHUNK_CUSTOM_TYPE) || line.contains(COMMIT_CUSTOM_TYPE) {
                assert!(line.contains("xxxxxxxx"));
                line.replacen("xxxxxxxx", "POISONED", 1)
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert_eq!(changed.len(), raw.len());
    assert_ne!(changed, raw);
    // Same open file/generation, same offsets, different digest. Any hydration
    // of a reserved row now fails, even if the caller later discards that row.
    std::fs::write(path, changed).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_context_and_harness_hooks_never_decode_archival_bodies() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp
        .path()
        .join("cold.jsonl")
        .to_string_lossy()
        .into_owned();
    let session = disk(&path, temp.path()).await;
    user(&session, "prompt").await;
    custom(&session, "normal-1", "extension", false).await;
    archives(&session).await;
    custom(&session, "normal-2", "extension", false).await;
    let expected = session
        .storage()
        .context_on_branch(
            &BranchBounds::default(),
            &session.view("main").get_leaf_id().await.unwrap().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        session
            .find_entries(&EntryQuery::default())
            .await
            .unwrap()
            .len(),
        52
    );
    session.drain().await;
    drop(session);
    let session = cold(&path, temp.path()).await;
    poison_archival_payloads(&path);
    assert_eq!(
        session
            .storage()
            .context_on_branch(
                &BranchBounds::default(),
                &session.view("main").get_leaf_id().await.unwrap().unwrap()
            )
            .await
            .unwrap()
            .messages,
        expected.messages
    );
    let filtered = session
        .view("main")
        .find_entries_on_branch(
            &EntryQuery {
                exclude_tool_program_bodies: true,
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(semantic_ids(&filtered), ["prompt", "normal-1", "normal-2"]);
    let transforms = Arc::new(AtomicUsize::new(0));
    let projects = Arc::new(AtomicUsize::new(0));
    let options = hooks(
        semantic_ids(&filtered),
        transforms.clone(),
        projects.clone(),
    );
    let provider = FauxProvider::new(FauxScript::new().with_text("done"));
    let harness = AgentHarness::create(AgentHarnessOptions {
        session: session.clone(),
        allow_existing_session: true,
        model: provider.default_model().clone(),
        models: vec![provider],
        entry_transforms: options.entry_transforms,
        entry_projectors: options.entry_projectors,
        ..Default::default()
    })
    .await
    .unwrap();
    harness.prompt_messages(vec![]).await.unwrap();
    assert!(transforms.load(Ordering::SeqCst) > 0);
    assert!(projects.load(Ordering::SeqCst) >= 2);
    assert!(session.find_entries(&EntryQuery::default()).await.is_err());
    assert!(session
        .find_entries(&EntryQuery {
            custom_type: Some(COMMIT_CUSTOM_TYPE.into()),
            ..Default::default()
        })
        .await
        .is_err());
}

struct NeverExecute {
    schema: rpi_ai::types::Tool,
    entered: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl AgentTool for NeverExecute {
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
        self.entered.fetch_add(1, Ordering::SeqCst);
        panic!("uncertain root/child must not execute")
    }
}
#[tokio::test]
async fn recovery_preserves_never_replay_and_noncustom_boundaries() {
    for boundary in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp
            .path()
            .join("recover.jsonl")
            .to_string_lossy()
            .into_owned();
        let session = disk(&path, temp.path()).await;
        let base = |id: &str| RecordBase {
            id: id.into(),
            seq: 0,
            lane: "main".into(),
            timestamp: 1,
        };
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
        let mut assistant = faux_assistant_message("", StopReason::ToolUse);
        let mut call = faux_tool_call("effect", json!({}));
        if let rpi_ai::types::Content::ToolCall(call) = &mut call {
            call.id = "root".into();
        }
        assistant.content = vec![call];
        append(
            &session,
            "assistant",
            ProvisionedKind::Message {
                message: AgentMessage::Assistant(Box::new(assistant)),
                terminate: None,
            },
        )
        .await;
        session
            .append_record(LaneRecord::ToolStarted(ToolStartedRecord {
                base: base("started"),
                run_id: "old-run".into(),
                assistant_entry_id: "assistant".into(),
                tool_index: 0,
                tool_call_id: "root".into(),
                tool_name: "effect".into(),
                effective_args: json!({}),
                result_entry_id: "reserved-root".into(),
                replay: ToolReplay::Never,
            }))
            .await
            .unwrap();
        if boundary {
            append(
                &session,
                "cut",
                ProvisionedKind::Compaction {
                    summary: "boundary".into(),
                    retained_tail: vec![],
                    tokens_before: 1,
                    details: None,
                    usage: None,
                },
            )
            .await;
        }
        custom(&session, "child", "rpi.tool_program.child.begin.v1", false).await;
        archives(&session).await;
        session.drain().await;
        drop(session);
        let session = cold(&path, temp.path()).await;
        // The non-custom boundary prevents looking through to an older assistant.
        // Both the harness tip scans and frame-progress unknown-outcome repair
        // must filter archival payloads from metadata before reading bodies.
        poison_archival_payloads(&path);
        let entered = Arc::new(AtomicUsize::new(0));
        let tool = NeverExecute {
            entered: entered.clone(),
            schema: rpi_ai::types::Tool {
                name: "effect".into(),
                description: "test".into(),
                parameters: rpi_ai::types::Schema::new(json!({"type":"object"})),
                constrained_sampling: None,
            },
        };
        let harness = AgentHarness::create(AgentHarnessOptions {
            session: session.clone(),
            allow_existing_session: true,
            tools: vec![
                HarnessTool::new(Arc::new(tool)).with_replay(rpi_harness::types::ToolReplay::Safe)
            ],
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(entered.load(Ordering::SeqCst), 0);
        let root = session.get_entry("reserved-root").await.unwrap();
        if boundary {
            assert!(root.is_none(), "compaction must remain a recovery boundary");
        } else {
            assert!(
                matches!(root, Some(Entry::Message(message)) if matches!(&message.message, AgentMessage::ToolResult(result) if result.is_error))
            );
        }
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
        drop(harness);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_notice_lookup_does_not_hydrate_poisoned_archives() {
    use rpi_harness::agent_harness::{BranchWriteGuard, ScopedEntry};
    let temp = tempfile::tempdir().unwrap();
    let path = temp
        .path()
        .join("notice.jsonl")
        .to_string_lossy()
        .into_owned();
    let session = disk(&path, temp.path()).await;
    user(&session, "prompt").await;
    custom(&session, "notice-source", "extension", false).await;
    archives(&session).await;
    session.drain().await;
    drop(session);
    let session = cold(&path, temp.path()).await;
    poison_archival_payloads(&path);
    let harness = AgentHarness::create(AgentHarnessOptions {
        session: session.clone(),
        allow_existing_session: true,
        ..Default::default()
    })
    .await
    .unwrap();
    // Keep the notice queued so its initial ACK and duplicate ACK share the
    // same branch guard, matching ordinary active-run extension notice writes.
    session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: RecordBase {
                id: "notice-run".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 1,
            },
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
    let guard = BranchWriteGuard {
        session_id: session.get_metadata().await.unwrap().id,
        leaf_id: session.view("main").get_leaf_id().await.unwrap(),
        custom_type: "extension".into(),
        entry_id: Some("notice-source".into()),
    };
    let notice = || ScopedEntry::Notice {
        key: "archival-cold-notice".into(),
        message: rpi_harness::messages::create_custom_message(
            "notice",
            rpi_ai::UserContent::Text("semantic notice".into()),
            true,
            None,
            1,
        ),
    };
    let accepted = harness.append_scoped_entry(&guard, notice()).await.unwrap();
    assert_eq!(
        harness.append_scoped_entry(&guard, notice()).await.unwrap(),
        accepted
    );
    let queued = session
        .find_records(&RecordQuery {
            record_type: Some("queue_enqueued"),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(queued.len(), 1, "dedup must not enqueue twice");
    assert!(
        session.find_entries(&EntryQuery::default()).await.is_err(),
        "explicit archive reads still validate payload digests"
    );
}
