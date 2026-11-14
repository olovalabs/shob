// Included by jsonl_failure_regressions: reuse its independent filesystem probe.

#[tokio::test]
async fn progress_foreign_prefix_is_not_truncated_even_when_the_new_suffix_matches() {
    let (fs, gate, storage) = progress_fixture().await;
    let path = "/progress-session.jsonl.progress.jsonl";
    let before = fs.read_binary_file(path, None).await.unwrap();
    gate.corrupt_prefix.store(true, Ordering::SeqCst);
    assert!(storage
        .append_progress(vec![progress_payload("failed")])
        .await
        .is_err());
    let untouched = fs.read_binary_file(path, None).await.unwrap();
    assert_eq!(
        untouched.len(),
        before.len() + 7,
        "must not truncate a foreign prefix"
    );
    assert_eq!(untouched[0], b'[');
    assert!(storage.read_progress("progress-run").await.is_err());
    assert!(storage
        .append_progress(vec![progress_payload("blocked")])
        .await
        .is_err());
    assert_eq!(fs.read_binary_file(path, None).await.unwrap(), untouched);
    fs.write_file(path, FileContent::Bytes(before), None)
        .await
        .unwrap();
    assert_eq!(
        reopen_progress(fs)
            .await
            .read_progress("progress-run")
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn progress_cancelled_load_repair_cannot_truncate_a_later_acknowledged_append() {
    let (fs, gate, storage) = progress_fixture().await;
    drop(storage);
    fs.append_file(
        "/progress-session.jsonl.progress.jsonl",
        FileContent::Text("{torn".into()),
        None,
    )
    .await
    .unwrap();
    let reopened = reopen_progress(fs.clone()).await;
    gate.block_truncate.store(true, Ordering::SeqCst);
    let reader = reopened.clone();
    let reading = tokio::spawn(async move { reader.read_progress("progress-run").await });
    gate.entered.notified().await;
    reading.abort();
    let _ = reading.await;
    let append = reopened.append_progress(vec![progress_payload("after-repair")]);
    tokio::pin!(append);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), &mut append)
            .await
            .is_err()
    );
    gate.release.notify_one();
    append.await.unwrap();
    let frames = reopen_progress(fs)
        .await
        .read_progress("progress-run")
        .await
        .unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[1].base.id, "after-repair");
}

#[tokio::test]
async fn progress_complete_last_value_without_newline_is_normalized_before_append() {
    let (fs, _, storage) = progress_fixture().await;
    drop(storage);
    let path = "/progress-session.jsonl.progress.jsonl";
    let mut bytes = fs.read_binary_file(path, None).await.unwrap();
    assert_eq!(bytes.pop(), Some(b'\n'));
    fs.write_file(path, FileContent::Bytes(bytes), None)
        .await
        .unwrap();
    let reopened = reopen_progress(fs.clone()).await;
    assert_eq!(
        reopened.read_progress("progress-run").await.unwrap().len(),
        1
    );
    reopened
        .append_progress(vec![progress_payload("after-newline-repair")])
        .await
        .unwrap();
    assert_eq!(
        reopen_progress(fs)
            .await
            .read_progress("progress-run")
            .await
            .unwrap()
            .len(),
        2
    );
}
fn progress_payload(id: &str) -> AssistantFrameRecord {
    AssistantFrameRecord {
        base: base(id),
        run_id: "progress-run".into(),
        stream_index: 0,
        op: AssistantFrameOp::Append,
        frame: Some(serde_json::json!({"payload":"recover this".repeat(20)})),
    }
}
fn progress_clear(id: &str) -> AssistantFrameRecord {
    AssistantFrameRecord {
        op: AssistantFrameOp::ClearRun,
        frame: None,
        ..progress_payload(id)
    }
}
async fn progress_fixture() -> (Arc<dyn FileSystem>, Arc<Gate>, JsonlSessionStorage) {
    let gate = Arc::new(Gate::default());
    let fs: Arc<dyn FileSystem> = Arc::new(GatedFs {
        inner: Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into())),
        gate: gate.clone(),
    });
    let storage = JsonlSessionStorage::create(
        fs.clone(),
        "/progress-session.jsonl",
        header(),
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    )
    .await
    .unwrap();
    storage
        .append_progress(vec![progress_payload("seed")])
        .await
        .unwrap();
    (fs, gate, storage)
}
async fn reopen_progress(fs: Arc<dyn FileSystem>) -> JsonlSessionStorage {
    JsonlSessionStorage::load(
        fs,
        "/progress-session.jsonl",
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn progress_failed_appends_rollback_exact_prefixes_or_poison_until_reopen() {
    for mode in [1, 2, 3] {
        for refuse_truncate in [false, true] {
            let (fs, gate, storage) = progress_fixture().await;
            let before = fs
                .read_binary_file("/progress-session.jsonl.progress.jsonl", None)
                .await
                .unwrap();
            gate.fail_prefix.store(mode, Ordering::SeqCst);
            gate.truncate_fail.store(refuse_truncate, Ordering::SeqCst);
            assert!(storage
                .append_progress(vec![progress_payload("one"), progress_payload("two")])
                .await
                .is_err());
            if refuse_truncate {
                assert!(storage.read_progress("progress-run").await.is_err());
                assert!(storage
                    .append_progress(vec![progress_payload("blocked")])
                    .await
                    .is_err());
            } else {
                assert_eq!(
                    fs.read_binary_file("/progress-session.jsonl.progress.jsonl", None)
                        .await
                        .unwrap(),
                    before
                );
                assert_eq!(
                    storage.read_progress("progress-run").await.unwrap().len(),
                    1
                );
            }
            gate.truncate_fail.store(false, Ordering::SeqCst);
            let reopened = reopen_progress(fs.clone()).await;
            let frames = reopened.read_progress("progress-run").await.unwrap();
            assert_eq!(frames[0].base.id, "seed");
            assert!(frames.iter().all(|frame| frame.base.id != "blocked"));
            reopened
                .append_progress(vec![progress_clear("retire")])
                .await
                .unwrap();
            assert!(reopen_progress(fs)
                .await
                .read_progress("progress-run")
                .await
                .unwrap()
                .is_empty());
        }
    }
}

#[tokio::test]
async fn progress_checkpoint_failure_never_publishes_a_partial_live_set() {
    for mode in [0, 1, 2] {
        let (fs, gate, storage) = progress_fixture().await;
        if mode == 0 {
            gate.fail_stage.store(true, Ordering::SeqCst);
        } else {
            gate.rename_mode.store(mode, Ordering::SeqCst);
        }
        assert!(storage
            .append_progress(vec![progress_clear("retire")])
            .await
            .is_err());
        if mode == 0 {
            assert_eq!(
                storage.read_progress("progress-run").await.unwrap().len(),
                1
            );
        } else {
            assert!(
                storage.read_progress("progress-run").await.is_err(),
                "uncertain rename requires reopen"
            );
        }
        let reopened = reopen_progress(fs.clone()).await;
        assert_eq!(
            reopened.read_progress("progress-run").await.unwrap().len(),
            if mode == 2 { 0 } else { 1 }
        );
        reopened
            .append_progress(vec![progress_payload("next")])
            .await
            .unwrap();
        assert!(reopen_progress(fs)
            .await
            .read_progress("progress-run")
            .await
            .unwrap()
            .iter()
            .any(|frame| frame.base.id == "next"));
    }
}

#[tokio::test]
async fn progress_owned_append_survives_cancellation_and_drain_waits_for_it() {
    let (fs, gate, storage) = progress_fixture().await;
    gate.armed.store(true, Ordering::SeqCst);
    let writer = storage.clone();
    let pending = tokio::spawn(async move {
        writer
            .append_progress(vec![progress_payload("accepted")])
            .await
    });
    gate.entered.notified().await;
    pending.abort();
    let _ = pending.await;
    let drain = storage.drain();
    tokio::pin!(drain);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), &mut drain)
            .await
            .is_err()
    );
    gate.release.notify_one();
    drain.await;
    assert_eq!(
        storage.read_progress("progress-run").await.unwrap().len(),
        2
    );
    assert_eq!(
        reopen_progress(fs)
            .await
            .read_progress("progress-run")
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn progress_retirement_serializes_concurrent_append_and_survives_caller_abort() {
    let (fs, gate, storage) = progress_fixture().await;
    gate.block_rename.store(true, Ordering::SeqCst);
    let owner = storage.clone();
    let clearing =
        tokio::spawn(async move { owner.append_progress(vec![progress_clear("retire")]).await });
    gate.entered.notified().await;
    clearing.abort();
    let _ = clearing.await;
    let append = storage.append_progress(vec![progress_payload("after")]);
    tokio::pin!(append);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), &mut append)
            .await
            .is_err()
    );
    gate.release.notify_one();
    append.await.unwrap();
    let frames = reopen_progress(fs)
        .await
        .read_progress("progress-run")
        .await
        .unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].base.id, "after");
}

#[tokio::test]
async fn progress_loader_repairs_only_torn_last_records_and_rejects_corruption() {
    let (fs, _, storage) = progress_fixture().await;
    drop(storage);
    let path = "/progress-session.jsonl.progress.jsonl";
    let valid = fs.read_binary_file(path, None).await.unwrap();
    fs.append_file(path, FileContent::Text("{\"id\":\"torn".into()), None)
        .await
        .unwrap();
    assert_eq!(
        reopen_progress(fs.clone())
            .await
            .read_progress("progress-run")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(fs.read_binary_file(path, None).await.unwrap(), valid);
    for bad in ["{\"invalid\":true}\n", "{broken\n", "{broken\n{}\n"] {
        let mut bytes = valid.clone();
        bytes.extend_from_slice(bad.as_bytes());
        fs.write_file(path, FileContent::Bytes(bytes.clone()), None)
            .await
            .unwrap();
        assert!(reopen_progress(fs.clone())
            .await
            .read_progress("progress-run")
            .await
            .is_err());
        assert_eq!(
            fs.read_binary_file(path, None).await.unwrap(),
            bytes,
            "corruption must not be rewritten"
        );
    }
}
