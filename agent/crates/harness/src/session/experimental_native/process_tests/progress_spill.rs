//! Large stream replacement spill and prune commit boundaries, independent of history.
use super::*;

async fn large_baseline(root: &Path, prune: bool) -> (PathBuf, SessionMetadata) {
    let path = fixture(root).await;
    let s = open(&path).await;
    let metadata = s.metadata();
    let mut run = frame("large-run-1", "main", "run", 1);
    run.frame = Some(json!({"delta":"a".repeat(1024*1024)}));
    let mut keep = frame("large-keep-1", "main", "keep", 1);
    keep.frame = Some(json!({"delta":"b".repeat(1024*1024)}));
    let prepared = s.append_progress(vec![run, keep]).await.unwrap();
    assert_eq!(
        prepared.iter().map(|f| f.base.seq).collect::<Vec<_>>(),
        vec![4, 5]
    );
    if prune {
        s.append_record(record(
            "finish-progress-spill",
            "operation_finished",
            json!({"runId":"run","outcome":"completed"}),
        ))
        .await
        .unwrap();
    }
    assert_eq!(
        s.history_revision().await.unwrap(),
        if prune { 16 } else { 15 }
    );
    s.drain().await;
    drop(s);
    (path, metadata)
}
fn progress(snapshot: &Value) -> Value {
    let rows = snapshot["raw"]["auxiliary"].as_array().unwrap();
    let json = rows.iter().find(|row| row[0] == "progress").unwrap()[1]
        .as_str()
        .unwrap();
    assert!(
        json.len() <= 4 * 1024 * 1024,
        "actual progress JSON must stay within the existing auxiliary budget"
    );
    serde_json::from_str(json).unwrap()
}
fn expected_frame(id: &str, lane: &str, run: &str, index: u64, seq: u64, delta: &str) -> Value {
    json!({"id":id,"seq":seq,"timestamp":42,"lane":lane,"runId":run,
        "streamIndex":index,"op":"append","frame":{"delta":delta}})
}
fn history_unchanged(baseline: &Value, committed: &Value, changed_query: &str) {
    for table in [
        "root",
        "facts",
        "selection",
        "tool_keys",
        "navigation",
        "ui_summary",
        "checkpoint",
        "projection",
    ] {
        assert!(
            baseline["raw"][table] == committed["raw"][table],
            "progress changed history component {table}"
        );
    }
    let without_progress = |value: &Value| {
        value["raw"]["auxiliary"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row[0] != "progress")
            .cloned()
            .collect::<Vec<_>>()
    };
    assert!(
        without_progress(baseline) == without_progress(committed),
        "non-progress auxiliary state changed"
    );
    for (key, value) in baseline.as_object().unwrap() {
        if key != "raw" && key != changed_query {
            assert!(value == &committed[key], "unchanged query changed: {key}");
        }
    }
}

#[test]
fn process_large_progress_replace_spill_and_prune_commit_boundaries() {
    runtime().block_on(async {
        for prune in [false, true] {
            let kind = if prune {
                "prune-large-cache"
            } else {
                "progress-replace-spill"
            };
            let operation = if prune { "prune" } else { "progress" };
            for stage in ["before", "after"] {
                let dir = tempfile::tempdir().unwrap();
                let (path, metadata) = large_baseline(dir.path(), prune).await;
                let baseline = snapshot(&path).await;
                let p = progress(&baseline);
                assert_eq!(p["sequence"], 5);
                assert_eq!(p["frames"].as_array().unwrap().len(), 5);
                let expected_base = json!([
                    expected_frame("base", "main", "run", 0, 1, "base"),
                    expected_frame("keep", "main", "keep", 0, 2, "keep"),
                    expected_frame("side", "side", "run", 0, 3, "side"),
                    expected_frame("large-run-1", "main", "run", 1, 4, &"a".repeat(1024 * 1024)),
                    expected_frame(
                        "large-keep-1",
                        "main",
                        "keep",
                        1,
                        5,
                        &"b".repeat(1024 * 1024)
                    )
                ]);
                assert!(
                    p["frames"] == expected_base,
                    "baseline full frame contents mismatch"
                );
                std::fs::create_dir(dir.path().join("oracle")).unwrap();
                let control = dir.path().join("oracle/control.rpinative");
                std::fs::copy(&path, &control).unwrap();
                let s = open(&control).await;
                action(&s, kind).await;
                drop(s);
                let committed = snapshot(&control).await;
                history_unchanged(&baseline, &committed, "runProgress");
                assert!(
                    baseline != committed,
                    "progress oracle must actually change"
                );
                let p = progress(&committed);
                let retained_ids = if prune {
                    vec!["keep", "side", "large-keep-1"]
                } else {
                    vec!["base", "keep", "side", "large-keep-1"]
                };
                let mut expected_frames = retained_ids
                    .iter()
                    .map(|id| {
                        expected_base
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|f| f["id"] == *id)
                            .unwrap()
                            .clone()
                    })
                    .collect::<Vec<_>>();
                if !prune {
                    expected_frames.push(expected_frame(
                        "large-run-2",
                        "main",
                        "run",
                        2,
                        7,
                        &"r".repeat(1024 * 1024),
                    ));
                }
                assert!(
                    p["frames"] == json!(expected_frames),
                    "independent control full frame semantics mismatch"
                );
                assert_eq!(p["sequence"], if prune { 5 } else { 7 });
                assert_eq!(
                    p["frames"].as_array().unwrap().len(),
                    if prune { 3 } else { 5 }
                );
                if prune {
                    let ids = p["frames"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|f| f["id"].as_str().unwrap())
                        .collect::<Vec<_>>();
                    assert_eq!(ids, vec!["keep", "side", "large-keep-1"]);
                    assert_eq!(committed["runProgress"].as_array().unwrap().len(), 1);
                    assert_eq!(committed["runProgress"][0]["lane"], "side");
                } else {
                    assert_eq!(committed["runProgress"].as_array().unwrap().len(), 3);
                    let ids = p["frames"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|f| f["id"].as_str().unwrap())
                        .collect::<Vec<_>>();
                    assert_eq!(
                        ids,
                        vec!["base", "keep", "side", "large-keep-1", "large-run-2"]
                    );
                }
                let original = raw::main_bytes(&path);
                assert!(!raw::journal_path(&path).exists());
                let mut child = ChildProcess::spawn(
                    dir.path(),
                    "large-progress",
                    json!({
                        "path":path,"kind":kind,"stage":format!("{operation}:{stage}")
                    }),
                );
                let config = child.event("opened");
                assert_eq!(config["journal"], "delete");
                assert_eq!(config["synchronous"], 2);
                assert_eq!(config["cacheSize"], 8);
                assert_eq!(config["cacheSpill"], 8);
                assert_eq!(config["revision"], if prune { 16 } else { 15 });
                eprintln!("large progress {operation}/{stage}: {config}");
                child.send("execute");
                assert_eq!(
                    child.event("stopped")["stage"],
                    format!("{operation}:{stage}")
                );
                if stage == "before" && !prune {
                    let evidence = raw::evidence(&path, &original);
                    assert_eq!(evidence["page_size"], config["pageSize"]);
                    eprintln!("staged {operation} spill: {evidence}");
                } else if stage == "before" {
                    // This fixture did not establish old-page spill for prune. Keep
                    // its exact commit/crash oracle, but never turn mere file growth
                    // or a journal's presence into a successful spill assertion.
                    let staged = raw::main_bytes(&path);
                    let changed = original
                        .iter()
                        .zip(staged.iter())
                        .filter(|(a, b)| a != b)
                        .count();
                    eprintln!(
                        "prune diagnostic only, no spill claim: original_bytes_changed={changed}"
                    );
                } else {
                    assert!(!raw::journal_path(&path).exists());
                }
                child.kill_and_reap();
                if stage == "before" && !prune {
                    eprintln!(
                        "killed {operation}, before SQLite open: {}",
                        raw::evidence(&path, &original)
                    );
                }
                // Do not call snapshot/open helpers first: this must be the first
                // logical access capable of actually playing the hot journal.
                let repo =
                    ExperimentalNativeRepo::with_clock(dir.path().to_owned(), Arc::new(FixedClock))
                        .await
                        .unwrap();
                let recovered = repo.open(&metadata).await.unwrap();
                assert_eq!(
                    recovered.history_revision().await.unwrap(),
                    if prune { 16 } else { 15 }
                );
                drop(recovered);
                drop(repo);
                let actual = snapshot(&path).await;
                let expected = if stage == "before" {
                    &baseline
                } else {
                    &committed
                };
                assert!(
                    actual == *expected,
                    "large {operation}/{stage} complete oracle mismatch"
                );
                assert!(!raw::journal_path(&path).exists());
                let s = open(&path).await;
                let revision = s.history_revision().await.unwrap();
                let next = s
                    .append_progress(vec![frame("after-large-recovery", "main", "keep", 2)])
                    .await
                    .unwrap();
                assert_eq!(
                    next[0].base.seq,
                    if !prune && stage == "after" { 8 } else { 6 }
                );
                assert_eq!(s.history_revision().await.unwrap(), revision);
                s.drain().await;
                drop(s);
                let after = snapshot(&path).await;
                history_unchanged(&actual, &after, "keepProgress");
                let mut expected_progress = progress(&actual);
                expected_progress["sequence"] = json!(next[0].base.seq);
                let new_frame = expected_frame(
                    "after-large-recovery",
                    "main",
                    "keep",
                    2,
                    next[0].base.seq,
                    "after-large-recovery",
                );
                expected_progress["frames"]
                    .as_array_mut()
                    .unwrap()
                    .push(new_frame.clone());
                assert!(
                    progress(&after) == expected_progress,
                    "post-recovery full progress/allocator was not persisted"
                );
                let mut expected_keep = actual["keepProgress"].clone();
                expected_keep.as_array_mut().unwrap().push(new_frame);
                assert!(
                    after["keepProgress"] == expected_keep,
                    "post-recovery keep query mismatch"
                );
                drop(child);
                dir.close().unwrap();
            }
        }
    });
}
