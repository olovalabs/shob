//! Forced pager-spill evidence, restricted to closed synthetic fixtures.
use super::*;
#[path = "progress_spill.rs"]
mod progress_spill;
#[path = "spill_raw.rs"]
mod raw;
#[path = "spill_rejection.rs"]
mod rejection;

// Foreign-format fixtures cannot be opened by the native engine. This helper
// only creates a real staged SQLite update for rejection tests, not a native
// commit implementation or a recovery substitute.
pub(super) fn unsupported_writer(path: &Path, peer: &mut Peer) {
    let mut db =
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    db.execute_batch("PRAGMA cache_size=8; PRAGMA cache_spill=ON; PRAGMA cache_spill=8;")
        .unwrap();
    peer.emit("opened", Value::Null);
    assert_eq!(peer.command(), "execute");
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    // Dirty a distinct original page before overflowing facts, so evidence must
    // include a genuinely changed old page rather than only newly grown pages.
    tx.execute("UPDATE root SET revision=999", []).unwrap();
    tx.execute(
        "UPDATE facts SET body=?1 WHERE seq=3",
        ["x".repeat(2 * 1024 * 1024)],
    )
    .unwrap();
    peer.emit("stopped", json!({"stage":"unsupported:before"}));
    // Parent must kill at this point. Timeout/error is never synthesized as release.
    let command = peer.command();
    panic!("unsupported fixture writer must be killed, not commanded: {command}");
}

fn checkpoint(snapshot: &Value) -> u64 {
    let checkpoint: Value =
        serde_json::from_str(snapshot["raw"]["checkpoint"][0][1].as_str().unwrap()).unwrap();
    checkpoint["projection"]["seq"].as_u64().unwrap()
}

#[test]
fn process_large_spill_kill_recovery_and_commit_ack_loss() {
    runtime().block_on(async {
        for route in ["store", "repo", "list"] {
            for stage in ["before", "after"] {
                let dir = tempfile::tempdir().unwrap();
                let path = fixture(dir.path()).await;
                let trusted = open(&path).await;
                let metadata = trusted.metadata();
                let identity = trusted
                    .run(|store, _| Ok(store.identity.clone()))
                    .await
                    .unwrap();
                drop(trusted);
                let baseline = snapshot(&path).await;
                assert_eq!(baseline["raw"]["projection"]["seq"], 15);
                assert_eq!(checkpoint(&baseline), 0);
                // This copy precedes the writer child and has no active connection/journal.
                std::fs::create_dir(dir.path().join("oracle")).unwrap();
                let control = dir.path().join("oracle/control.rpinative");
                std::fs::copy(&path, &control).unwrap();
                let s = open(&control).await;
                action(&s, "history-large-spill").await;
                drop(s);
                let committed = snapshot(&control).await;
                assert_eq!(committed["raw"]["projection"]["seq"], 20);
                assert_eq!(checkpoint(&committed), 16);
                let encoded = committed["raw"]["facts"][15][7].as_str().unwrap();
                assert!(encoded.len() > 2 * 1024 * 1024 && encoded.len() <= engine::BODY_LIMIT);
                assert_eq!(baseline["pending"].as_array().unwrap().len(), 1);
                assert_eq!(committed["pending"], json!([]));
                assert_eq!(committed["open"], json!([]));
                assert!(!committed["raw"]["tool_keys"].as_array().unwrap().is_empty());
                assert!(
                    baseline != committed,
                    "large oracle must actually change state"
                );
                let original = raw::main_bytes(&path);
                assert!(!raw::journal_path(&path).exists());
                let mut child = ChildProcess::spawn(
                    dir.path(),
                    "large-spill",
                    json!({
                        "path":path,"kind":"history-large-spill","stage":format!("history:{stage}")
                    }),
                );
                let config = child.event("opened");
                assert_eq!(config["revision"], 15);
                assert_eq!(config["journal"], "delete");
                assert_eq!(config["synchronous"], 2);
                assert_eq!(config["cacheSize"], 8);
                assert_eq!(config["cacheSpill"], 8);
                eprintln!("large spill {route}/{stage}: {config}");
                child.send("execute");
                assert_eq!(child.event("stopped")["stage"], format!("history:{stage}"));
                if stage == "before" {
                    // No parent SQLite connection is opened while the writer is paused.
                    // OS reads cannot play the rollback journal or release the probe.
                    let evidence = raw::evidence(&path, &original);
                    assert_eq!(evidence["page_size"], config["pageSize"]);
                    eprintln!("staged rollback evidence: {evidence}");
                } else {
                    assert!(
                        !raw::journal_path(&path).exists(),
                        "DELETE commit must finish journal retirement"
                    );
                }
                child.kill_and_reap();
                if stage == "before" {
                    // The killed process cannot run transaction destructors; this journal
                    // still protects main DB bytes that differ from the closed baseline.
                    let evidence = raw::evidence(&path, &original);
                    eprintln!("after kill, before any SQLite open: {evidence}");
                }
                // First logical read after kill must itself be capable of recovery.
                // In particular, do not hide a repo bug by first calling snapshot(),
                // whose existing helper begins with a read-only identity probe.
                let recovered = if route == "store" {
                    ExperimentalNativeStorage::from_store(
                        Store::open(&path, identity).unwrap(),
                        metadata,
                        Arc::new(FixedClock),
                    )
                } else {
                    let repo = ExperimentalNativeRepo::with_clock(
                        dir.path().to_owned(),
                        Arc::new(FixedClock),
                    )
                    .await
                    .unwrap();
                    if route == "list" {
                        let catalog = repo.list().await.unwrap();
                        assert_eq!(catalog, vec![metadata.clone()]);
                    }
                    repo.open(&metadata).await.unwrap_or_else(|error| {
                        panic!("existing experimental repo failed crash recovery: {error}")
                    })
                };
                assert_eq!(
                    recovered.history_revision().await.unwrap(),
                    if stage == "before" { 15 } else { 20 }
                );
                drop(recovered);
                let actual = snapshot(&path).await;
                let expected = if stage == "before" {
                    &baseline
                } else {
                    &committed
                };
                // Full Value equality, not length/hash comparison. Do not Debug-dump a
                // multi-MiB canonical body if the oracle fails.
                assert!(
                    actual == *expected,
                    "large-spill full-state mismatch: {route}/{stage}"
                );
                assert!(
                    !raw::journal_path(&path).exists(),
                    "recovery must retire DELETE journal"
                );
                let s = open(&path).await;
                s.set_name(Some("next after large fault")).await.unwrap();
                assert_eq!(
                    s.history_revision().await.unwrap(),
                    if stage == "before" { 16 } else { 21 }
                );
                drop(s);
                drop(child);
                dir.close().unwrap();
            }
        }
    });
}
