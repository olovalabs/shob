use super::*;

#[tokio::test]
async fn native_commits_share_the_guard_gate_and_recovery_refuses_scoped_mutations() {
    let h = harness().await;
    let initial = guard(&h).await;
    let gate = h.message_writes.lock().await;
    let entry = ProvisionedEntry {
        id: "native".into(),
        kind: ProvisionedKind::Message {
            message: notice(),
            terminate: None,
        },
    };
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(10),
        h.append_provisioned(entry.clone())
    )
    .await
    .is_err());
    assert!(h.session.get_entry("native").await.unwrap().is_none());
    drop(gate);
    h.append_provisioned(entry).await.unwrap();
    assert!(h
        .append_scoped_entry(
            &initial,
            ScopedEntry::Custom {
                custom_type: "goal-state".into(),
                data: None
            }
        )
        .await
        .is_err());
    let current = guard(&h).await;
    let (run_id, _, _) = h.acquire_run(OperationKind::Run).await.unwrap();
    assert!(h
        .append_scoped_entry(
            &current,
            ScopedEntry::Custom {
                custom_type: "goal-state".into(),
                data: None
            }
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("recovery in progress"));
    assert!(h.navigate_tree(None, false, None, None).await.is_err());
    h.release_run(&run_id);
    assert!(h
        .append_scoped_entry(
            &current,
            ScopedEntry::Custom {
                custom_type: "goal-state".into(),
                data: None
            }
        )
        .await
        .is_ok());
}

#[tokio::test]
async fn durable_jsonl_outbox_reopens_without_reaccepting_a_delivered_notice() {
    use crate::session::jsonl::{
        JsonlSessionCreateOptions, JsonlSessionRepo, JsonlSessionRepoOptions,
    };
    use crate::session::memory::SystemClock;
    use crate::session::session::DefaultIdGenerator;
    let temporary = tempfile::tempdir().unwrap();
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
            temporary.path().to_owned(),
        )),
        sessions_root: temporary.path().to_string_lossy().into_owned(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });
    let storage = repo
        .create_typed(&JsonlSessionCreateOptions {
            id: Some("outbox-reopen".into()),
            cwd: temporary.path().to_string_lossy().into_owned(),
            metadata: None,
            parent_session_id: None,
        })
        .await
        .unwrap();
    let metadata = storage.jsonl_metadata();
    let session = Session::new(Arc::new(storage), None);
    let h = AgentHarness::create(AgentHarnessOptions {
        session,
        ..Default::default()
    })
    .await
    .unwrap();
    let source = state(&h).await;
    h.session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: RecordBase {
                id: "interrupted".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 0,
            },
            source_leaf_id: Some(source),
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        }))
        .await
        .unwrap();
    let accepted = deliver(&h).await;
    assert!(
        h.session.get_entry(&accepted).await.unwrap().is_none(),
        "accepted outbox target must still be queued at crash"
    );
    drop(h);
    for _ in 0..2 {
        let session = Session::new(
            Arc::new(repo.open_by_jsonl_metadata(&metadata).await.unwrap()),
            None,
        );
        let h = AgentHarness::create(AgentHarnessOptions {
            session,
            allow_existing_session: true,
            ..Default::default()
        })
        .await
        .unwrap();
        assert_eq!(deliver(&h).await, accepted);
        let messages = h
            .view()
            .find_entries_on_branch(
                &EntryQuery {
                    outbox_key: Some("revision-1".into()),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
            .unwrap();
        assert_eq!(messages.len(), 1);
    }
}

#[tokio::test]
async fn compaction_reissues_current_notice_instead_of_deduping_against_pruned_context() {
    let h = harness().await;
    state(&h).await;
    let before = deliver(&h).await;
    h.session
        .append_entry(
            ProvisionedEntry {
                id: "compact".into(),
                kind: ProvisionedKind::Compaction {
                    summary: "earlier discussion".into(),
                    retained_tail: vec![],
                    tokens_before: 1,
                    details: None,
                    usage: None,
                },
            },
            "main",
        )
        .await
        .unwrap();
    let after = deliver(&h).await;
    assert_ne!(before, after);
    assert_eq!(deliver(&h).await, after);
}

async fn harness() -> AgentHarness {
    AgentHarness::create(AgentHarnessOptions::default())
        .await
        .unwrap()
}
async fn guard(h: &AgentHarness) -> BranchWriteGuard {
    BranchWriteGuard {
        session_id: h.session.storage().metadata().id,
        leaf_id: h.view().get_leaf_id().await.unwrap(),
        custom_type: "goal-state".into(),
        entry_id: h
            .view()
            .find_entries_on_branch(
                &EntryQuery {
                    custom_type: Some("goal-state".into()),
                    order: Some(EntryOrder::NewestFirst),
                    limit: Some(1),
                    ..Default::default()
                },
                &BranchBounds::default(),
            )
            .await
            .unwrap()
            .first()
            .map(|entry| entry.id().to_owned()),
    }
}
async fn state(h: &AgentHarness) -> String {
    h.append_scoped_entry(
        &guard(h).await,
        ScopedEntry::Custom {
            custom_type: "goal-state".into(),
            data: Some(serde_json::json!({"title":"A"})),
        },
    )
    .await
    .unwrap()
}
fn notice() -> AgentMessage {
    crate::messages::create_custom_message(
        "goal",
        UserContent::Text("Goal A".into()),
        true,
        None,
        1,
    )
}
async fn deliver(h: &AgentHarness) -> String {
    h.append_scoped_entry(
        &guard(h).await,
        ScopedEntry::Notice {
            message: notice(),
            key: "revision-1".into(),
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn scoped_writes_reject_stale_session_leaf_and_state() {
    let h = harness().await;
    let initial = guard(&h).await;
    state(&h).await;
    let change = || ScopedEntry::Custom {
        custom_type: "goal-state".into(),
        data: None,
    };
    assert!(h.append_scoped_entry(&initial, change()).await.is_err());
    let mut current = guard(&h).await;
    current.session_id.push_str("-different");
    assert!(h.append_scoped_entry(&current, change()).await.is_err());
    current = guard(&h).await;
    current.entry_id = None;
    assert!(h.append_scoped_entry(&current, change()).await.is_err());
    assert!(h
        .append_scoped_entry(&guard(&h).await, change())
        .await
        .is_ok());
}

#[tokio::test]
async fn outbox_deduplication_is_branch_local_and_rejects_payload_collisions() {
    let h = harness().await;
    let source = state(&h).await;
    let first = deliver(&h).await;
    assert_eq!(deliver(&h).await, first);
    assert!(h
        .append_scoped_entry(
            &guard(&h).await,
            ScopedEntry::Notice {
                message: crate::messages::create_custom_message(
                    "goal",
                    UserContent::Text("Changed".into()),
                    true,
                    None,
                    1
                ),
                key: "revision-1".into(),
            }
        )
        .await
        .is_err());
    h.navigate_tree(Some(&source), false, None, None)
        .await
        .unwrap();
    let other = deliver(&h).await;
    assert_ne!(
        other, first,
        "a notice consumed on another branch cannot suppress this branch"
    );
    h.navigate_tree(Some(&first), false, None, None)
        .await
        .unwrap();
    assert_eq!(deliver(&h).await, first);
}

#[tokio::test]
async fn queued_outbox_survives_reconstruction_and_does_not_drain_on_another_branch() {
    let h = harness().await;
    let source = state(&h).await;
    h.session
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: RecordBase {
                id: "run".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 0,
            },
            source_leaf_id: Some(source.clone()),
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        }))
        .await
        .unwrap();
    let accepted = deliver(&h).await;
    assert!(h.session.get_entry(&accepted).await.unwrap().is_none());
    assert_eq!(deliver(&h).await, accepted);
    h.navigate_tree(None, false, None, None).await.unwrap();
    assert!(h.pending_context_entries().await.unwrap().is_empty());
    h.navigate_tree(Some(&source), false, None, None)
        .await
        .unwrap();
    assert_eq!(h.pending_context_entries().await.unwrap()[0].id, accepted);
    let session = h.session.clone();
    drop(h);
    let reopened = AgentHarness::create(AgentHarnessOptions {
        session,
        allow_existing_session: true,
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(deliver(&reopened).await, accepted);
    assert!(reopened
        .session
        .get_entry(&accepted)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn competing_guarded_writers_accept_only_one_mutation() {
    let h = harness().await;
    let initial = guard(&h).await;
    let write = || {
        h.append_scoped_entry(
            &initial,
            ScopedEntry::Custom {
                custom_type: "goal-state".into(),
                data: None,
            },
        )
    };
    let (a, b) = tokio::join!(write(), write());
    assert_ne!(a.is_ok(), b.is_ok());
}

mod scoped_drop_probe {
    use super::*;
    use crate::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
    use crate::session::memory::SystemClock;
    use crate::session::session::DefaultIdGenerator;
    use rpi_tools::env::AppendFile;
    use rpi_tools::{FileContent, FileError, FileSystem};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::sync::Notify;

    #[derive(Default)]
    struct AppendGate {
        used: AtomicBool,
        entered: Notify,
        release: Notify,
    }

    // Real JSONL append barrier: cancel only the API caller while the storage
    // owner retains its tail reservation and commits the enqueue.
    struct GatedAppend {
        fs: Arc<dyn FileSystem>,
        path: String,
        gate: Arc<AppendGate>,
    }

    #[async_trait::async_trait]
    impl AppendFile for GatedAppend {
        async fn append(&mut self, bytes: &[u8]) -> Result<(), FileError> {
            if String::from_utf8_lossy(bytes).contains("queue_enqueued")
                && !self.gate.used.swap(true, Ordering::SeqCst)
            {
                self.gate.entered.notify_one();
                self.gate.release.notified().await;
            }
            self.fs
                .append_file(&self.path, FileContent::Bytes(bytes.to_vec()), None)
                .await
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_scoped_notice_caller_schedules_context_control() {
        tokio::time::timeout(Duration::from_secs(20), probe(false))
            .await
            .expect("control timed out: append owner, admission gate, or retry did not finish");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_scoped_notice_retry_must_schedule_context() {
        tokio::time::timeout(Duration::from_secs(20), probe(true))
            .await
            .expect("probe timed out: append owner, admission gate, or retry did not finish");
    }

    async fn probe(drop_caller: bool) {
        let dir = match std::env::var_os("RPI_NOTICE_TEST_ROOT") {
            Some(root) => tempfile::tempdir_in(root).unwrap(),
            None => tempfile::tempdir().unwrap(),
        };
        let path = dir
            .path()
            .join("scoped-drop.jsonl")
            .to_string_lossy()
            .replace('\\', "/");
        let fs: Arc<dyn FileSystem> =
            Arc::new(rpi_tools::OsExecutionEnv::with_cwd(dir.path().to_owned()));
        let storage = Arc::new(
            JsonlSessionStorage::create(
                fs.clone(),
                &path,
                JsonlV4Header::new(
                    "scoped-drop-probe".into(),
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
            .unwrap(),
        );
        let h = AgentHarness::create(AgentHarnessOptions {
            session: Session::new(storage.clone(), None),
            ..Default::default()
        })
        .await
        .unwrap();
        let source = state(&h).await;
        let before_open = guard(&h).await;

        // A real open operation suppresses the scoped notice's idle flush.
        // It is appended AFTER harness creation, so startup recovery cannot
        // consume it or rebuild the in-process pending-context hint.
        h.session
            .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
                base: RecordBase {
                    id: "held-open-operation".into(),
                    seq: 0,
                    lane: "main".into(),
                    timestamp: 0,
                },
                source_leaf_id: Some(source.clone()),
                intent: OperationIntent::Run {
                    original_prompt: vec![],
                    initial_messages: vec![],
                    system_prompt_override: None,
                    resume_data: None,
                },
            }))
            .await
            .unwrap();
        let initial = guard(&h).await;
        assert_eq!(initial.leaf_id, before_open.leaf_id);
        assert_eq!(initial.entry_id.as_deref(), Some(source.as_str()));
        assert_eq!(initial.entry_id, before_open.entry_id);
        assert!(
            !h.has_pending_context(),
            "setup unexpectedly scheduled context"
        );
        let revision = h.session.storage().history_revision().await.unwrap();

        let gate = Arc::new(AppendGate::default());
        storage.install_append_handle(Box::new(GatedAppend {
            fs: fs.clone(),
            path: path.clone(),
            gate: gate.clone(),
        }));
        let caller = {
            let h = h.clone();
            let initial = initial.clone();
            tokio::spawn(async move {
                h.append_scoped_entry(
                    &initial,
                    ScopedEntry::Notice {
                        message: notice(),
                        key: "revision-1".into(),
                    },
                )
                .await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
            .await
            .expect("scoped notice never reached the real JSONL enqueue append");
        // The backend owner is now inside append(), holding its real tail
        // reservation. Abort ONLY the API caller, not the storage-owned task.
        let caller = if drop_caller {
            caller.abort();
            assert!(caller.await.unwrap_err().is_cancelled());
            None
        } else {
            Some(caller)
        };
        gate.release.notify_one();
        h.session.drain().await; // tail barrier includes reducer commit, not just disk append
        let live_ack = match caller {
            Some(caller) => Some(caller.await.unwrap().unwrap()),
            None => None,
        };

        let query = RecordQuery {
            lane: Some("main".into()),
            record_type: Some("queue_enqueued"),
            queue_kind: Some(QueueKind::Context),
            pending_queue_only: true,
            ..Default::default()
        };
        let records = h.session.find_records(&query).await.unwrap();
        assert_eq!(
            records.len(),
            1,
            "storage owner did not commit exactly one pending enqueue"
        );
        let LaneRecord::QueueEnqueued(enqueue) = &records[0] else {
            unreachable!()
        };
        let original_id = enqueue.target["id"]
            .as_str()
            .expect("enqueue target must carry its reserved entry ID")
            .to_owned();
        if let Some(ack) = live_ack {
            assert_eq!(ack, original_id);
        }
        assert_eq!(
            h.session.storage().history_revision().await.unwrap(),
            revision + 1,
            "only queue_enqueued may advance history; operation/source were already committed"
        );
        assert!(
            h.session.get_entry(&original_id).await.unwrap().is_none(),
            "held-open operation must prevent idle delivery"
        );
        let bytes = fs.read_text_file(&path, None).await.unwrap();
        assert_eq!(
            bytes
                .lines()
                .filter(|line| line.contains("queue_enqueued"))
                .count(),
            1,
            "the enqueue must actually exist in the temporary JSONL file"
        );

        let fresh = guard(&h).await;
        if drop_caller {
            assert!(
                !h.has_pending_context(),
                "setup must reproduce the missing hint"
            );
            let mut stale = fresh.clone();
            stale.session_id.push_str("-stale");
            assert!(h
                .append_scoped_entry(
                    &stale,
                    ScopedEntry::Notice {
                        message: notice(),
                        key: "revision-1".into(),
                    }
                )
                .await
                .is_err());
            let mut changed = notice();
            if let AgentMessage::Custom(custom) = &mut changed {
                custom.data["changed"] = serde_json::json!(true);
            }
            assert!(h
                .append_scoped_entry(
                    &fresh,
                    ScopedEntry::Notice {
                        message: changed,
                        key: "revision-1".into(),
                    }
                )
                .await
                .is_err());
            assert!(h
                .append_scoped_entry(
                    &fresh,
                    ScopedEntry::Notice {
                        message: notice(),
                        key: String::new(),
                    }
                )
                .await
                .is_err());
            assert!(
                !h.has_pending_context(),
                "rejected guards and payloads must not schedule"
            );
        }
        assert_eq!(fresh.session_id, initial.session_id);
        assert_eq!(
            fresh.leaf_id, initial.leaf_id,
            "enqueue must not move the branch leaf"
        );
        assert_eq!(
            fresh.entry_id, initial.entry_id,
            "source guard must remain valid"
        );
        let retry_id = h
            .append_scoped_entry(
                &fresh,
                ScopedEntry::Notice {
                    message: notice(),
                    key: "revision-1".into(),
                },
            )
            .await
            .expect("retry of the committed pending notice must return Ok(original_id)");
        assert_eq!(
            retry_id, original_id,
            "retry must deduplicate to the original reserved ID"
        );
        assert_eq!(
            h.session.storage().history_revision().await.unwrap(),
            revision + 1,
            "dedup retry must not append another record or deliver while the operation is open"
        );
        assert_eq!(h.session.find_records(&query).await.unwrap().len(), 1);
        assert!(h.session.get_entry(&original_id).await.unwrap().is_none());
        assert_eq!(
            h.session
                .find_open_operations("main", None)
                .await
                .unwrap()
                .len(),
            1
        );
        let pending = h.pending_context_entries().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, original_id);

        eprintln!("scoped-drop probe: dropped={drop_caller}, retry=Ok({retry_id}), durable_pending={}, scheduled={}",
            pending.len(), h.has_pending_context());
        // Desired contract, deliberately NOT inverted to accept the bug.
        // This is the exact private predicate used by context_boundary_hook's
        // fully-committed-prefix fast path; no provider/configuration is needed.
        assert!(
            h.has_pending_context(),
            "BUG: successful scoped Notice retry acknowledged original ID {original_id}, \
             but its durable pending context has no delivery scheduling hint \
             (dropped_caller={drop_caller}); a fully committed next-turn boundary skips it"
        );

        // Exercise the actual fast-path hook with an entirely committed (empty)
        // prefix. The restored hint must cause delivery, not just flip a bit.
        let settle = Arc::new(crate::settle::SettleState::new(
            "held-open-operation",
            false,
            0,
            Default::default(),
        ));
        let error = Arc::new(Mutex::new(None));
        let signal = CancellationToken::new();
        let hook = h.context_boundary_hook(None, settle, signal.clone(), error.clone());
        let mut assistant = AssistantMessage::empty(rpi_ai::Api::Faux, "faux", "faux", 0);
        assistant.stop_reason = rpi_ai::StopReason::Stop;
        let context = rpi_agent::AgentContext::default();
        let update = hook(ShouldStopAfterTurnContext {
            message: &assistant,
            tool_results: &[],
            context: &context,
            new_messages: &[],
        })
        .await
        .expect("scheduled context must update the fully committed boundary");
        assert_eq!(update.context.unwrap().messages.len(), 1);
        assert!(error.lock().unwrap().is_none());
        assert!(!signal.is_cancelled());
        assert!(h.session.get_entry(&original_id).await.unwrap().is_some());
        assert!(h.pending_context_entries().await.unwrap().is_empty());
        assert!(!h.has_pending_context());
        let delivered_revision = h.session.storage().history_revision().await.unwrap();
        assert_eq!(deliver(&h).await, original_id);
        assert!(
            !h.has_pending_context(),
            "consumed branch dedup must not fabricate work"
        );
        assert_eq!(
            h.session.storage().history_revision().await.unwrap(),
            delivered_revision
        );
        assert!(
            hook(ShouldStopAfterTurnContext {
                message: &assistant,
                tool_results: &[],
                context: &context,
                new_messages: &[],
            })
            .await
            .is_none(),
            "consumed notice must not be delivered again"
        );
        let bytes = fs.read_text_file(&path, None).await.unwrap();
        assert_eq!(
            bytes
                .lines()
                .filter(|line| line.contains("queue_enqueued"))
                .count(),
            1
        );
        assert_eq!(
            bytes
                .lines()
                .filter(|line| line.contains("queue_cancelled"))
                .count(),
            0
        );
    }
}

#[tokio::test]
async fn guarded_custom_appends_cannot_mint_native_journal_on_main_or_side() {
    for lane in ["main", "side"] {
        let mut h = harness().await;
        if lane != "main" {
            h.session.create_lane(lane, None).await.unwrap();
            h.lane = lane.into();
        }
        for typ in [
            "rpi.tool_program.child_begin.v1",
            "rpi.tool_program.child_phase.v1",
        ] {
            let before = h.session.storage().history_revision().await.unwrap();
            let guard = BranchWriteGuard {
                session_id: h.session.storage().metadata().id,
                leaf_id: h.view().get_leaf_id().await.unwrap(),
                custom_type: typ.into(),
                entry_id: None,
            };
            let error = h
                .append_scoped_entry(
                    &guard,
                    ScopedEntry::Custom {
                        custom_type: typ.into(),
                        data: Some(serde_json::json!({"childId":"forged"})),
                    },
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("reserved native child journal"));
            assert_eq!(
                h.session.storage().history_revision().await.unwrap(),
                before
            );
        }
        let typ = "rpi.tool_program.child_begin.v1.other";
        let guard = BranchWriteGuard {
            session_id: h.session.storage().metadata().id,
            leaf_id: h.view().get_leaf_id().await.unwrap(),
            custom_type: typ.into(),
            entry_id: None,
        };
        h.append_scoped_entry(
            &guard,
            ScopedEntry::Custom {
                custom_type: typ.into(),
                data: None,
            },
        )
        .await
        .unwrap();
    }
}
