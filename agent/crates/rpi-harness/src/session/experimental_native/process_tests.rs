//! Disposable process-fault fixtures; no CLI routing or product recovery API.
use super::*;
use crate::session::{experimental_native::ExperimentalNativeRepo, memory::Clock};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Mutex as StdMutex,
    time::Duration,
};
#[path = "process_tests/ipc.rs"]
mod ipc;
use ipc::{ChildProcess, Peer};
#[path = "process_tests/spill_tests.rs"]
mod spill_tests;

struct FixedClock;
impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        42
    }
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
fn user(id: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.into(),
        kind: ProvisionedKind::Message {
            message: rpi_ai::UserMessage::new(format!("synthetic {id}"), 42).into(),
            terminate: None,
        },
    }
}
fn record(id: &str, kind: &str, extra: Value) -> LaneRecord {
    let mut value = json!({"id":id,"type":kind,"seq":0,"timestamp":0,"lane":"main"});
    value
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    serde_json::from_value(value).unwrap()
}
fn frame(id: &str, lane: &str, run: &str, index: u32) -> AssistantFrameRecord {
    serde_json::from_value(json!({"id":id,"seq":0,"timestamp":0,"lane":lane,
        "runId":run,"streamIndex":index,"op":"append","frame":{"delta":id}}))
    .unwrap()
}
fn history_batch() -> Vec<Write> {
    let tool = serde_json::from_value(json!({"kind":"tool_result","role":"toolResult",
        "toolCallId":"call","toolName":"fixture","content":[],"isError":false,"timestamp":42}))
    .unwrap();
    vec![
        Write::Entry(user("queued"), "main".into()),
        Write::Entry(
            ProvisionedEntry {
                id: "tool-result".into(),
                kind: ProvisionedKind::Message {
                    message: tool,
                    terminate: None,
                },
            },
            "main".into(),
        ),
        Write::Record(record(
            "finish",
            "operation_finished",
            json!({"runId":"run","outcome":"completed"}),
        )),
        Write::Name(Some("committed batch".into())),
        Write::Label("queued".into(), Some("label".into())),
    ]
}
fn large_history_batch() -> Vec<Write> {
    let mut writes = history_batch();
    writes[0] = Write::Entry(
        ProvisionedEntry {
            id: "queued".into(),
            kind: ProvisionedKind::Message {
                message: rpi_ai::UserMessage::new("z".repeat(2 * 1024 * 1024), 42).into(),
                terminate: None,
            },
        },
        "main".into(),
    );
    writes
}
async fn open(path: &Path) -> ExperimentalNativeStorage {
    // Deliberately bypass the process-local repo registry: every verifier owns a new connection.
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let identity: Identity = db
        .query_row("SELECT session,generation FROM root", [], |r| {
            Ok(Identity {
                session: r.get(0)?,
                generation: r.get(1)?,
            })
        })
        .unwrap();
    let metadata = auxiliary(&db, &identity, "metadata").unwrap();
    drop(db);
    ExperimentalNativeStorage::from_store(
        Store::open(path, identity).unwrap(),
        metadata,
        Arc::new(FixedClock),
    )
}
async fn fixture(root: &Path) -> PathBuf {
    let repo = ExperimentalNativeRepo::with_clock(root.to_owned(), Arc::new(FixedClock))
        .await
        .unwrap();
    let storage = repo
        .create(&SessionCreateOptions {
            id: Some("fixture".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    storage.run(|s,clock| {
        let mut writes=vec![Write::Record(record("run","operation_started",json!({"sourceLeafId":null,"intent":{"kind":"run","originalPrompt":[],"initialMessages":[]}}))),Write::Lane("side".into(),None,true)];
        writes.extend((0..12).map(|n|Write::Entry(user(&format!("baseline-{n}")),"main".into())));
        writes.push(Write::Record(record("enqueue","queue_enqueued",json!({"queue":"steer","target":{"id":"queued","type":"message","message":{"kind":"user","role":"user","content":"queued","timestamp":42}}}))));
        commit(s,writes,Some(0),clock)?;
        Ok(())
    }).await.unwrap();
    assert_eq!(storage.history_revision().await.unwrap(), 15);
    storage
        .append_progress(vec![
            frame("base", "main", "run", 0),
            frame("keep", "main", "keep", 0),
            frame("side", "side", "run", 0),
        ])
        .await
        .unwrap();
    storage.drain().await;
    drop(storage);
    drop(repo);
    root.join("fixture.rpinative")
}
async fn action(s: &ExperimentalNativeStorage, kind: &str) {
    match kind {
        "history" => {
            s.run(|store, clock| commit(store, history_batch(), Some(15), clock))
                .await
                .unwrap();
        }
        "history-large-spill" => {
            s.run(|store, clock| commit(store, large_history_batch(), Some(15), clock))
                .await
                .unwrap();
        }
        "progress-replace-spill" => {
            // A related stream replacement changes existing payload pages. A pure
            // append can grow overflow pages without modifying any old page before
            // COMMIT, and must not be mislabeled as proven old-page spill.
            let mut clear = frame("clear-large-run-1", "main", "run", 1);
            clear.op = AssistantFrameOp::ClearStream;
            clear.frame = None;
            let mut large = frame("large-run-2", "main", "run", 2);
            large.frame = Some(json!({"delta":"r".repeat(1024*1024)}));
            let prepared = s.append_progress(vec![clear, large]).await.unwrap();
            assert_eq!(
                prepared.iter().map(|f| f.base.seq).collect::<Vec<_>>(),
                vec![6, 7]
            );
        }
        "prune-large-cache" => {
            assert_eq!(s.prune_completed_progress().await.unwrap(), 2);
        }
        "progress" => {
            s.append_progress(vec![
                frame("next-1", "main", "run", 1),
                frame("next-2", "main", "run", 2),
            ])
            .await
            .unwrap();
        }
        "prune" => {
            assert_eq!(s.prune_completed_progress().await.unwrap(), 1);
        }
        _ => panic!("unknown action {kind}"),
    }
}
fn table(db: &Connection, name: &str, order: &str) -> Value {
    let mut stmt = db
        .prepare(&format!("SELECT * FROM {name} ORDER BY {order}"))
        .unwrap();
    let columns = stmt.column_count();
    let rows = stmt
        .query_map([], |row| {
            use rusqlite::types::ValueRef;
            Ok(Value::Array(
                (0..columns)
                    .map(|i| match row.get_ref(i).unwrap() {
                        ValueRef::Null => Value::Null,
                        ValueRef::Integer(v) => json!(v),
                        ValueRef::Real(v) => json!(v),
                        ValueRef::Text(v) => json!(std::str::from_utf8(v).unwrap()),
                        ValueRef::Blob(v) => json!({"blob":v}),
                    })
                    .collect(),
            ))
        })
        .unwrap();
    Value::Array(rows.map(|row| row.unwrap()).collect())
}
async fn snapshot(path: &Path) -> Value {
    let s = open(path).await;
    let raw = s
        .run(|store, _| {
            let (projection, reads) = store.restore()?;
            assert!(reads.tail_rows <= engine::TAIL as usize);
            for seq in 1..=projection.seq {
                engine::read_mutation(&store.connection, &store.identity, seq)?;
            }
            let mut progress: ProgressState =
                auxiliary(&store.connection, &store.identity, "progress")?;
            progress.validate().unwrap();
            let mut result = json!({"projection":projection});
            let mut schema=store.connection.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?;
            let names=schema.query_map([],|row|row.get::<_,String>(0))?.collect::<std::result::Result<Vec<_>,_>>()?;
            assert_eq!(names, ["auxiliary","checkpoint","facts","navigation","root","selection","tool_keys","ui_summary"],"new tables must be included in the full-row oracle");
            for (name, order) in [
                ("root", "singleton"),
                ("facts", "seq"),
                ("selection", "seq"),
                ("tool_keys", "tool,seq"),
                ("navigation", "seq"),
                ("ui_summary", "seq"),
                ("checkpoint", "singleton"),
                ("auxiliary", "key"),
            ] {
                result[name] = table(&store.connection, name, order);
            }
            Ok(result)
        })
        .await
        .unwrap();
    let window = s
        .message_window(
            "main",
            &MessageWindowQuery {
                anchor_id: Some("baseline-11".into()),
                limit: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let window = json!({"entries":window.entries,"first":window.first_id,"last":window.last_id,
        "nextBefore":window.next_before_id,"nextAfter":window.next_after_id,
        "moreBefore":window.has_more_before,"moreAfter":window.has_more_after,"leaf":window.leaf_id});
    let value = json!({"raw":raw,"log":s.get_log(&Default::default()).await.unwrap(),
        "lanes":s.get_lanes().await.unwrap(),"stats":s.get_stats().await.unwrap(),
        "pending":s.find_records(&RecordQuery{pending_queue_only:true,..Default::default()}).await.unwrap(),
        "open":s.find_open_operations("main",None).await.unwrap(),
        "summary":s.message_summaries("main",&Default::default()).await.unwrap(),
        "window":window,
        "runProgress":s.read_progress("run").await.unwrap(),"keepProgress":s.read_progress("keep").await.unwrap()});
    s.drain().await;
    drop(s);
    value
}

#[test]
#[ignore = "guarded helper, launched only by the process-fault parents"]
fn child() {
    let Some((mut peer, payload)) = Peer::from_env() else {
        return;
    };
    let path: PathBuf = serde_json::from_value(payload["path"].clone()).unwrap();
    peer.validate_artifact(&path);
    let kind = payload["kind"].as_str().unwrap().to_owned();
    let stage = payload["stage"].as_str().unwrap().to_owned();
    let rt = runtime();
    if kind == "unsupported-spill" {
        spill_tests::unsupported_writer(&path, &mut peer);
        return;
    }
    let s = rt.block_on(open(&path));
    if matches!(
        kind.as_str(),
        "history-large-spill" | "progress-replace-spill" | "prune-large-cache"
    ) {
        rt.block_on(s.run(|store, _| {
            store.connection.execute_batch(
                "PRAGMA main.cache_size=8; PRAGMA cache_spill=ON; PRAGMA main.cache_spill=8;",
            )?;
            Ok(())
        }))
        .unwrap();
    }
    let pragmas=rt.block_on(s.run(|store,_|Ok(json!({
        "sqlite":rusqlite::version(),
        "journal":store.connection.query_row("PRAGMA journal_mode",[],|r|r.get::<_,String>(0))?,
        "synchronous":store.connection.query_row("PRAGMA synchronous",[],|r|r.get::<_,i64>(0))?,
        "cacheSize":store.connection.query_row("PRAGMA cache_size",[],|r|r.get::<_,i64>(0))?,
        "cacheSpill":store.connection.query_row("PRAGMA cache_spill",[],|r|r.get::<_,i64>(0))?,
        "pageSize":store.connection.query_row("PRAGMA page_size",[],|r|r.get::<_,i64>(0))?,
        "revision":store.restore()?.0.seq,
    })))).unwrap();
    peer.emit("opened", pragmas);
    assert_eq!(peer.command(), "execute");
    let peer = Arc::new(StdMutex::new(peer));
    let control = peer.clone();
    rt.block_on(s.run(move |store, _| {
        store.process_probe = Some(Arc::new(move |actual| {
            if actual == stage {
                let mut peer = control.lock().unwrap();
                peer.emit("stopped", json!({"stage":actual}));
                assert_eq!(peer.command(), "release");
            }
        }));
        Ok(())
    }))
    .unwrap();
    if kind == "lock-holder" {
        rt.block_on(s.run(|store, clock| {
            commit(
                store,
                vec![Write::Name(Some("holder".into()))],
                Some(15),
                clock,
            )
        }))
        .unwrap();
    } else if kind == "lock-contender" {
        // Typed busy evidence is checked before the SessionError string adapter.
        rt.block_on(s.run(|store,clock| {
            store.connection.busy_timeout(Duration::ZERO)?;
            let error=commit(store,vec![Write::Name(Some("must-not-exist".into()))],Some(15),clock).unwrap_err();
            assert!(matches!(error.downcast_ref::<rusqlite::Error>(),Some(rusqlite::Error::SqliteFailure(e,_)) if e.code==rusqlite::ErrorCode::DatabaseBusy),"expected SQLITE_BUSY, got {error}");
            Ok(())
        })).unwrap();
        peer.lock().unwrap().emit("busy", Value::Null);
        assert_eq!(peer.lock().unwrap().command(), "retry");
        let expected = payload["expected"].as_u64().unwrap();
        rt.block_on(s.run(move |store, clock| {
            if expected != 15 {
                let error = commit(
                    store,
                    vec![Write::Name(Some("stale-must-not-exist".into()))],
                    Some(15),
                    clock,
                )
                .unwrap_err();
                assert!(error.to_string().contains("revision changed"));
            }
            let result = commit(
                store,
                vec![Write::Name(Some("contender".into()))],
                Some(expected),
                clock,
            )?;
            assert_eq!(result[0].seq(), expected + 1);
            Ok(())
        }))
        .unwrap();
    } else {
        rt.block_on(action(&s, &kind));
    }
    peer.lock().unwrap().emit(
        "result",
        json!({"revision":rt.block_on(s.history_revision()).unwrap()}),
    );
    assert_eq!(peer.lock().unwrap().command(), "finish");
    rt.block_on(s.drain());
    drop(s);
    drop(rt);
}

#[test]
fn process_history_batch_kill_and_commit_ack_loss_are_atomic() {
    runtime().block_on(async {
        for stage in ["before","after","success"] {
            let dir=tempfile::tempdir().unwrap();let path=fixture(dir.path()).await;
            // The fixture is closed before copying. This is a synthetic same-identity
            // oracle clone, not a migration/fork or a copy of a live SQLite artifact.
            let control=dir.path().join("control.rpinative");std::fs::copy(&path,&control).unwrap();
            let baseline=snapshot(&path).await;
            let oracle=open(&control).await;action(&oracle,"history").await;drop(oracle);
            let committed=snapshot(&control).await;
            assert_ne!(baseline,committed);
            assert_eq!(baseline["raw"]["projection"]["seq"],15);
            assert_eq!(committed["raw"]["projection"]["seq"],20);
            let baseline_checkpoint:Value=serde_json::from_str(baseline["raw"]["checkpoint"][0][1].as_str().unwrap()).unwrap();
            let committed_checkpoint:Value=serde_json::from_str(committed["raw"]["checkpoint"][0][1].as_str().unwrap()).unwrap();
            assert_eq!(baseline_checkpoint["projection"]["seq"],0);
            assert_eq!(committed_checkpoint["projection"]["seq"],16);
            assert_eq!(baseline["pending"].as_array().unwrap().len(),1);
            assert_eq!(committed["pending"],json!([]));
            assert_eq!(baseline["open"].as_array().unwrap().len(),1);
            assert_eq!(committed["open"],json!([]));
            assert!(!committed["raw"]["tool_keys"].as_array().unwrap().is_empty());
            let mut child=ChildProcess::spawn(dir.path(),"history",json!({"path":path,"kind":"history","stage":format!("history:{}",if stage=="success" {"after"} else {stage})}));
            let opened=child.event("opened");eprintln!("process history {stage}: {opened}");assert_eq!(opened["revision"],15);
            child.send("execute");assert_eq!(child.event("stopped")["stage"],format!("history:{}",if stage=="success" {"after"} else {stage}));
            if stage=="success" {child.send("release");assert_eq!(child.event("result")["revision"],20);child.send("finish");child.finish();}
            else {child.kill_and_reap();}
            let expected=if stage=="before" {&baseline} else {&committed};
            assert_eq!(&snapshot(&path).await,expected,"history stage {stage}");
            let s=open(&path).await;
            assert_eq!(s.history_revision().await.unwrap(),if stage=="before" {15} else {20});
            s.set_name(Some("next")).await.unwrap();
            assert_eq!(s.history_revision().await.unwrap(),if stage=="before" {16} else {21});
            drop(s);drop(child);dir.close().unwrap();
        }
    });
}
#[test]
fn process_progress_and_prune_kill_matrix_is_independent_of_history() {
    runtime().block_on(async {
        for kind in ["progress", "prune"] {
            for stage in ["before", "after"] {
                let dir = tempfile::tempdir().unwrap();
                let path = fixture(dir.path()).await;
                if kind == "prune" {
                    let s = open(&path).await;
                    action(&s, "history").await;
                    drop(s);
                }
                let baseline = snapshot(&path).await;
                let control = dir.path().join("control.rpinative");
                std::fs::copy(&path, &control).unwrap();
                let s = open(&control).await;
                action(&s, kind).await;
                drop(s);
                let committed = snapshot(&control).await;
                assert_eq!(baseline["raw"]["facts"], committed["raw"]["facts"]);
                assert_eq!(
                    baseline["raw"]["checkpoint"],
                    committed["raw"]["checkpoint"]
                );
                assert_eq!(baseline["raw"]["root"], committed["raw"]["root"]);
                assert_eq!(baseline["keepProgress"], committed["keepProgress"]);
                if kind == "prune" {
                    assert_eq!(committed["runProgress"].as_array().unwrap().len(), 1);
                    assert_eq!(committed["runProgress"][0]["lane"], "side");
                } else {
                    assert_eq!(committed["runProgress"].as_array().unwrap().len(), 4);
                }
                assert_ne!(baseline, committed);
                let mut child = ChildProcess::spawn(
                    dir.path(),
                    kind,
                    json!({"path":path,"kind":kind,"stage":format!("{kind}:{stage}")}),
                );
                child.event("opened");
                child.send("execute");
                assert_eq!(child.event("stopped")["stage"], format!("{kind}:{stage}"));
                child.kill_and_reap();
                assert_eq!(
                    snapshot(&path).await,
                    if stage == "before" {
                        baseline
                    } else {
                        committed
                    },
                    "{kind}:{stage}"
                );
                let s = open(&path).await;
                let revision = s.history_revision().await.unwrap();
                let next = s
                    .append_progress(vec![frame("after-recovery", "main", "keep", 1)])
                    .await
                    .unwrap();
                assert_eq!(
                    next[0].base.seq,
                    if kind == "progress" && stage == "after" {
                        6
                    } else {
                        4
                    }
                );
                assert_eq!(s.history_revision().await.unwrap(), revision);
                drop(s);
                drop(child);
                dir.close().unwrap();
            }
        }
    });
}
fn panic_text(error: Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = error.downcast_ref::<String>() {
        text.clone()
    } else if let Some(text) = error.downcast_ref::<&str>() {
        (*text).to_owned()
    } else {
        panic!("unexpected non-text panic payload")
    }
}
#[test]
fn process_ipc_wrong_stage_is_failure_and_unwind_reaps_the_writer() {
    runtime().block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path()).await;
        let baseline = snapshot(&path).await;
        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut child = ChildProcess::spawn(
                dir.path(),
                "wrong-stage",
                json!({
                    "path":path,"kind":"lock-holder","stage":"history:before"
                }),
            );
            child.event("opened");
            child.send("execute");
            // Drop during unwind must kill/reap the child still holding the transaction.
            child.event("wrong-stage-must-fail");
        }));
        let failure = panic_text(failure.expect_err("wrong stage was accepted"));
        assert!(
            failure.contains("expected \"wrong-stage-must-fail\", got \"stopped\""),
            "{failure}"
        );
        assert_eq!(snapshot(&path).await, baseline);
        let s = open(&path).await;
        s.set_name(Some("lock released after unwind"))
            .await
            .unwrap();
        assert_eq!(s.history_revision().await.unwrap(), 16);
        drop(s);
        dir.close().unwrap();
    });
}
#[test]
fn process_ipc_helper_panic_is_not_accepted_as_an_expected_crash() {
    runtime().block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path()).await;
        let baseline = snapshot(&path).await;
        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut child = ChildProcess::spawn(
                dir.path(),
                "panic-child",
                json!({
                    "path":path,"kind":"invalid-action-must-panic","stage":"unused"
                }),
            );
            child.event("opened");
            child.send("execute");
            // A helper panic/exit 101 is not proof of an expected transaction phase.
            child.event("stopped");
        }));
        let failure = panic_text(failure.expect_err("helper panic was accepted"));
        assert!(
            failure.contains("premature child exit")
                && failure.contains("unknown action invalid-action-must-panic"),
            "{failure}"
        );
        assert_eq!(snapshot(&path).await, baseline);
        dir.close().unwrap();
    });
}
#[test]
fn process_sqlite_writer_busy_then_release_or_kill_has_no_sequence_holes() {
    runtime().block_on(async {
        for kill in [false,true] {
            let dir=tempfile::tempdir().unwrap();let path=fixture(dir.path()).await;
            let control=dir.path().join("control.rpinative");std::fs::copy(&path,&control).unwrap();
            let s=open(&control).await;
            if !kill {s.set_name(Some("holder")).await.unwrap();}
            s.set_name(Some("contender")).await.unwrap();drop(s);
            let expected=snapshot(&control).await;
            let mut holder=ChildProcess::spawn(dir.path(),"holder",json!({"path":path,"kind":"lock-holder","stage":"history:before"}));
            let mut contender=ChildProcess::spawn(dir.path(),"contender",json!({"path":path,"kind":"lock-contender","stage":"unused","expected":if kill {15} else {16}}));
            holder.event("opened");contender.event("opened");
            holder.send("execute");assert_eq!(holder.event("stopped")["stage"],"history:before");
            contender.send("execute");contender.event("busy");
            if kill {holder.kill_and_reap();}else{holder.send("release");assert_eq!(holder.event("result")["revision"],16);holder.send("finish");holder.finish();}
            contender.send("retry");assert_eq!(contender.event("result")["revision"],if kill {16} else {17});contender.send("finish");contender.finish();
            assert_eq!(snapshot(&path).await,expected,"holder killed={kill}");
            drop(contender);drop(holder);dir.close().unwrap();
        }
    });
}
