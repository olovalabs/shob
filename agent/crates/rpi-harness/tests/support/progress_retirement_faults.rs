async fn terminal_progress_fixture() -> (Arc<dyn FileSystem>, Arc<Gate>, JsonlSessionStorage) {
    let (fs, gate, storage) = progress_fixture().await;
    storage
        .append_progress(vec![AssistantFrameRecord {
            run_id: "unknown".into(),
            ..progress_payload("survivor")
        }])
        .await
        .unwrap();
    storage
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            base: base("finished"),
            run_id: "progress-run".into(),
            outcome: OperationOutcome::Completed,
            error: None,
            result: None,
        }))
        .await
        .unwrap();
    (fs, gate, storage)
}

#[tokio::test]
async fn failed_terminal_cleanup_does_not_publish_a_session_switch_and_reopen_retries() {
    use rpi_harness::agent_harness::AgentHarness;
    use rpi_harness::session::Session;
    use rpi_harness::types::AgentHarnessOptions;
    for mode in [0, 1, 2] {
        let (fs, gate, storage) = terminal_progress_fixture().await;
        let main = fs
            .read_binary_file("/progress-session.jsonl", None)
            .await
            .unwrap();
        let harness = AgentHarness::create(Default::default()).await.unwrap();
        let old_id = harness.session().get_metadata().await.unwrap().id;
        if mode == 0 {
            gate.fail_stage.store(true, Ordering::SeqCst);
        } else {
            gate.rename_mode.store(mode, Ordering::SeqCst);
        }
        let target = Session::new(Arc::new(storage.clone()), None);
        assert!(harness.set_session(target).await.is_err());
        assert_eq!(harness.session().get_metadata().await.unwrap().id, old_id);
        if mode == 0 {
            assert_eq!(
                storage.read_progress("progress-run").await.unwrap().len(),
                1
            );
        } else {
            assert!(storage.read_progress("progress-run").await.is_err());
        }
        let reopened = reopen_progress(fs.clone()).await;
        let restored = AgentHarness::create(AgentHarnessOptions {
            session: Session::new(Arc::new(reopened), None),
            allow_existing_session: true,
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(restored
            .session()
            .read_progress("progress-run")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            restored.session().read_progress("unknown").await.unwrap()[0]
                .base
                .id,
            "survivor"
        );
        assert_eq!(
            fs.read_binary_file("/progress-session.jsonl", None)
                .await
                .unwrap(),
            main
        );
        let reopened = reopen_progress(fs).await;
        gate.fail_stage.store(true, Ordering::SeqCst);
        assert_eq!(reopened.prune_completed_progress().await.unwrap(), 0);
        assert!(
            gate.fail_stage.load(Ordering::SeqCst),
            "no-op cleanup must not attempt checkpoint staging"
        );
    }
}

#[tokio::test]
async fn terminal_cleanup_owned_publication_survives_cancellation() {
    let (fs, gate, storage) = terminal_progress_fixture().await;
    gate.block_rename.store(true, Ordering::SeqCst);
    let writer = storage.clone();
    let pruning = tokio::spawn(async move { writer.prune_completed_progress().await });
    gate.entered.notified().await;
    pruning.abort();
    let _ = pruning.await;
    let drain = storage.drain();
    tokio::pin!(drain);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), &mut drain)
            .await
            .is_err()
    );
    gate.release.notify_one();
    drain.await;
    let reopened = reopen_progress(fs).await;
    assert_eq!(reopened.prune_completed_progress().await.unwrap(), 0);
    assert!(reopened
        .read_progress("progress-run")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(reopened.read_progress("unknown").await.unwrap().len(), 1);
}
