//! Conditional extension acceptance must be atomic with native history.
mod sqlite_race {
    use rpi_harness::agent_harness::{AgentHarness, BranchWriteGuard, ScopedEntry};
    use rpi_harness::session::memory::Clock;
    use rpi_harness::session::types::{
        EntryQuery, SessionCreateOptions, SessionRepo, SessionStorage,
    };
    use rpi_harness::session::{DefaultIdGenerator, Session, SqliteSessionRepo};
    use rpi_harness::types::AgentHarnessOptions;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Barrier,
    };

    struct GatedClock {
        armed: AtomicBool,
        count: AtomicUsize,
        barrier: Barrier,
    }
    impl Clock for GatedClock {
        fn now_ms(&self) -> i64 {
            if self.armed.load(Ordering::SeqCst) && self.count.fetch_add(1, Ordering::SeqCst) < 2 {
                self.barrier.wait();
            }
            1
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn scoped_compare_and_append_must_recheck_after_a_sqlite_cas_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(GatedClock {
            armed: AtomicBool::new(false),
            count: AtomicUsize::new(0),
            barrier: Barrier::new(2),
        });
        let db = dir.path().join("scoped.sqlite");
        let a = SqliteSessionRepo::with_clock_ids(
            &db,
            clock.clone(),
            Arc::new(DefaultIdGenerator::new()),
        )
        .unwrap();
        let first = a
            .create(&SessionCreateOptions {
                id: Some("shared".into()),
                parent_session_id: None,
                metadata: None,
            })
            .await
            .unwrap();
        let metadata = first.metadata();
        let b = SqliteSessionRepo::with_clock_ids(
            &db,
            clock.clone(),
            Arc::new(DefaultIdGenerator::new()),
        )
        .unwrap();
        let second = b.open(&metadata).await.unwrap();
        let h1 = AgentHarness::create(AgentHarnessOptions {
            session: Session::new(Arc::new(first), None),
            ..Default::default()
        })
        .await
        .unwrap();
        let h2 = AgentHarness::create(AgentHarnessOptions {
            session: Session::new(Arc::new(second), None),
            ..Default::default()
        })
        .await
        .unwrap();
        let guard = BranchWriteGuard {
            session_id: "shared".into(),
            leaf_id: None,
            custom_type: "goal-state".into(),
            entry_id: None,
        };
        clock.armed.store(true, Ordering::SeqCst);
        let g1 = guard.clone();
        let g2 = guard.clone();
        let l = tokio::spawn(async move {
            h1.append_scoped_entry(
                &g1,
                ScopedEntry::Custom {
                    custom_type: "goal-state".into(),
                    data: Some(serde_json::json!({"goal":"A"})),
                },
            )
            .await
        });
        let r = tokio::spawn(async move {
            h2.append_scoped_entry(
                &g2,
                ScopedEntry::Custom {
                    custom_type: "goal-state".into(),
                    data: Some(serde_json::json!({"goal":"B"})),
                },
            )
            .await
        });
        let (l, r) = tokio::join!(l, r);
        let (l, r) = (l.unwrap(), r.unwrap());
        clock.armed.store(false, Ordering::SeqCst);
        let entries = a
            .open(&metadata)
            .await
            .unwrap()
            .find_entries(&EntryQuery::default())
            .await
            .unwrap();
        eprintln!(
            "same expected branch guard: left={l:?} right={r:?}; persisted_entries={}",
            entries.len()
        );
        assert_eq!(
            usize::from(l.is_ok()) + usize::from(r.is_ok()),
            1,
            "only one writer may accept the same expected branch state"
        );
    }
}
mod jsonl_race {
    use async_trait::async_trait;
    use rpi_ai::providers::faux::{FauxProvider, FauxScript};
    use rpi_harness::agent_harness::{AgentHarness, AgentLane, BranchWriteGuard, ScopedEntry};
    use rpi_harness::session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        memory::{CounterIdGenerator, FakeClock},
        Session,
    };
    use rpi_harness::types::AgentHarnessOptions;
    use rpi_tools::env::AppendFile;
    use rpi_tools::{FileContent, FileError, FileInfo, FileSystem};
    use std::{
        future::{poll_fn, Future},
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        task::Poll,
    };
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
            self.inner
                .append_file(&self.path, FileContent::Bytes(bytes.to_vec()), None)
                .await?;
            if String::from_utf8_lossy(bytes).contains("second-user-input")
                && !String::from_utf8_lossy(bytes).contains("operation_started")
                && self.gate.armed.swap(false, Ordering::SeqCst)
            {
                self.gate.entered.notify_one();
                self.gate.release.notified().await;
            }
            Ok(())
        }
    }
    include!("support/scoped_write_fs.rs");

    #[tokio::test]
    async fn idle_scoped_writes_reject_a_stale_leaf_and_accept_a_fresh_one() {
        let h = AgentHarness::create(Default::default()).await.unwrap();
        let mut guard = BranchWriteGuard {
            session_id: h.session().storage().metadata().id,
            leaf_id: h.get_leaf_id().await.unwrap(),
            custom_type: "goal-state".into(),
            entry_id: None,
        };
        h.append_message(rpi_ai::UserMessage::new("intervening input", 0).into())
            .await
            .unwrap();
        let entry = || ScopedEntry::Custom {
            custom_type: "goal-state".into(),
            data: None,
        };
        assert!(h.append_scoped_entry(&guard, entry()).await.is_err());
        guard.leaf_id = h.get_leaf_id().await.unwrap();
        assert!(h.append_scoped_entry(&guard, entry()).await.is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn same_harness_scoped_guard_must_not_be_invalidated_by_a_pending_native_prompt_append() {
        tokio::time::timeout(std::time::Duration::from_secs(10),async {
 let temp=tempfile::tempdir().unwrap();
 let path=temp.path().join("session.jsonl").to_string_lossy().replace('\\',"/");
 let gate=Arc::new(Gate::default());
 let fs:Arc<dyn FileSystem>=Arc::new(GatedFs{inner:Arc::new(rpi_tools::OsExecutionEnv::with_cwd(temp.path().into())),gate:gate.clone()});
 let storage=JsonlSessionStorage::create(fs,&path,JsonlV4Header::new("one-harness".into(),1,temp.path().display().to_string(),None,None,None),Arc::new(FakeClock::new()),Arc::new(CounterIdGenerator::new())).await.unwrap();
 let provider=FauxProvider::new(FauxScript::new().with_text("done"));
 let h=AgentHarness::create(AgentHarnessOptions{session:Session::new(Arc::new(storage),None),model:provider.default_model().clone(),models:vec![provider],..Default::default()}).await.unwrap();
 gate.armed.store(true,Ordering::SeqCst);
 let runner=h.clone();
 let native=tokio::spawn(async move{runner.prompt_messages(vec![rpi_ai::UserMessage::new("first-user-input",0).into(),rpi_ai::UserMessage::new("second-user-input",0).into()]).await});
 gate.entered.notified().await;
 let guard=BranchWriteGuard{session_id:h.session().storage().metadata().id,leaf_id:h.get_leaf_id().await.unwrap(),custom_type:"goal-state".into(),entry_id:None};
 let expected_leaf=guard.leaf_id.clone();
 let mut guarded=Box::pin(h.append_scoped_entry(&guard,ScopedEntry::Custom{custom_type:"goal-state".into(),data:Some(serde_json::json!({"active":"A"}))}));
 // Poll exactly once: all in-memory guard reads complete; append then waits for
 // the tail reservation held by the blocked native append. No sleep-based race.
 poll_fn(|cx|match guarded.as_mut().poll(cx){Poll::Pending=>Poll::Ready(()),Poll::Ready(r)=>panic!("guarded append unexpectedly completed before releasing native append: {r:?}")}).await;
 gate.release.notify_one();
 let result=guarded.await;
 let mut actual_parent=None;
 if let Ok(id)=&result{actual_parent=h.session().get_entry(id).await.unwrap().unwrap().parent_id().map(str::to_owned);}
 let native_result=native.await.unwrap();
 eprintln!("same Harness/JSONL: expected_leaf={expected_leaf:?}, actual_parent={actual_parent:?}, scoped={result:?}, native_ok={}",native_result.is_ok());
 assert!(native_result.is_ok());
 assert!(result.is_err(),"a native input append invalidated the expected leaf after guard validation; scoped write must conflict");
 }).await.expect("bounded probe must not hang");
    }
}
