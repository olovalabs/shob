#[cfg(test)]
mod tests {
    use rpi_harness::session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        memory::{CounterIdGenerator, FakeClock},
        types::*,
    };
    use rpi_tools::FileSystem;
    use std::sync::Arc;

    fn base(id: &str, lane: &str) -> RecordBase {
        RecordBase {
            id: id.into(),
            seq: 0,
            lane: lane.into(),
            timestamp: 0,
        }
    }
    fn start(id: &str, lane: &str) -> LaneRecord {
        LaneRecord::OperationStarted(OperationStartedRecord {
            base: base(id, lane),
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        })
    }
    fn finish(id: &str, run: &str, lane: &str) -> LaneRecord {
        LaneRecord::OperationFinished(OperationFinishedRecord {
            base: base(id, lane),
            run_id: run.into(),
            outcome: OperationOutcome::Completed,
            error: None,
            result: None,
        })
    }
    fn frame(id: &str) -> LaneRecord {
        LaneRecord::AssistantFrame(AssistantFrameRecord {
            base: base(id, "main"),
            run_id: "seed".into(),
            stream_index: 0,
            op: AssistantFrameOp::ClearRun,
            frame: None,
        })
    }
    fn entry(id: &str) -> ProvisionedEntry {
        ProvisionedEntry {
            id: id.into(),
            kind: ProvisionedKind::Message {
                message: rpi_ai::UserMessage::new(id, 0).into(),
                terminate: None,
            },
        }
    }
    async fn fixture() -> (Arc<dyn FileSystem>, JsonlSessionStorage) {
        let fs: Arc<dyn FileSystem> =
            Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
        let storage = JsonlSessionStorage::create(
            fs.clone(),
            "/triple.jsonl",
            JsonlV4Header::new("triple".into(), 1, "/".into(), None, None, None),
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .await
        .unwrap();
        (fs, storage)
    }
    #[tokio::test]
    async fn triple_rejects_second_start_before_writing_any_bytes() {
        let (fs, storage) = fixture().await;
        let before = fs.read_text_file("/triple.jsonl", None).await.unwrap();
        let result = storage
            .append_record_entry_and_record(
                start("a", "main"),
                entry("message"),
                "main",
                start("b", "main"),
            )
            .await;
        eprintln!(
            "triple second start: result_ok={}, open_ops={}",
            result.is_ok(),
            storage
                .find_open_operations("main", None)
                .await
                .unwrap()
                .len()
        );
        assert!(
            result.is_err(),
            "triple accepted two open operations on one lane"
        );
        assert_eq!(
            fs.read_text_file("/triple.jsonl", None).await.unwrap(),
            before
        );
    }
    #[tokio::test]
    async fn triple_checks_after_start_against_existing_operation() {
        let (fs, storage) = fixture().await;
        storage.append_record(start("seed", "main")).await.unwrap();
        let before = fs.read_text_file("/triple.jsonl", None).await.unwrap();
        let result = storage
            .append_record_entry_and_record(
                frame("frame"),
                entry("message"),
                "main",
                start("new", "main"),
            )
            .await;
        assert!(
            result.is_err(),
            "triple's after record bypassed existing-open-operation validation"
        );
        assert_eq!(
            fs.read_text_file("/triple.jsonl", None).await.unwrap(),
            before
        );
    }
    #[tokio::test]
    async fn triple_wrong_finish_does_not_free_the_operation_slot() {
        let (fs, storage) = fixture().await;
        storage.append_record(start("seed", "main")).await.unwrap();
        let before = fs.read_text_file("/triple.jsonl", None).await.unwrap();
        let result = storage
            .append_record_entry_and_record(
                finish("finish", "wrong", "main"),
                entry("message"),
                "main",
                start("new", "main"),
            )
            .await;
        assert!(
            result.is_err(),
            "a finish for another run must not permit a second start"
        );
        assert_eq!(
            fs.read_text_file("/triple.jsonl", None).await.unwrap(),
            before
        );
    }
    #[tokio::test]
    async fn triple_matching_finish_then_start_and_cross_lane_starts_are_valid() {
        let (fs, storage) = fixture().await;
        storage.append_record(start("seed", "main")).await.unwrap();
        storage
            .append_record_entry_and_record(
                finish("finish", "seed", "main"),
                entry("message"),
                "main",
                start("new", "main"),
            )
            .await
            .unwrap();
        storage.create_lane("other", None).await.unwrap();
        storage
            .append_record_entry_and_record(
                finish("finish2", "new", "main"),
                entry("message2"),
                "main",
                start("other-run", "other"),
            )
            .await
            .unwrap();
        storage
            .append_record_entry_and_record(
                start("another", "main"),
                entry("message3"),
                "main",
                frame("last-frame"),
            )
            .await
            .unwrap();
        assert_eq!(
            storage
                .find_open_operations("main", None)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            storage
                .find_open_operations("other", None)
                .await
                .unwrap()
                .len(),
            1
        );
        let reloaded = JsonlSessionStorage::load(
            fs,
            "/triple.jsonl",
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .await
        .unwrap();
        assert_eq!(
            storage.get_log(&LogOptions::default()).await.unwrap(),
            reloaded.get_log(&LogOptions::default()).await.unwrap()
        );
    }
    #[tokio::test]
    async fn triple_matches_sequential_validation_matrix() {
        use rpi_harness::session::memory::InMemorySessionStorage;
        fn record(choice: usize, prefix: &str) -> LaneRecord {
            let id = format!("{prefix}-{choice}");
            match choice {
                0 => frame(&id),
                1 => start(&id, "main"),
                2 => start(&id, "other"),
                3 => finish(&id, "seed-main", "main"),
                4 => finish(&id, "seed-other", "other"),
                _ => finish(&id, "wrong", "main"),
            }
        }
        for seeded in 0..4 {
            for left in 0..6 {
                for right in 0..6 {
                    let (fs, jsonl) = fixture().await;
                    let memory = InMemorySessionStorage::new(
                        jsonl.metadata(),
                        Arc::new(FakeClock::new()),
                        Arc::new(CounterIdGenerator::new()),
                    );
                    jsonl.create_lane("other", None).await.unwrap();
                    memory.create_lane("other", None).await.unwrap();
                    for (mask, lane) in [(1, "main"), (2, "other")] {
                        if seeded & mask != 0 {
                            let seed = start(&format!("seed-{lane}"), lane);
                            jsonl.append_record(seed.clone()).await.unwrap();
                            memory.append_record(seed).await.unwrap();
                        }
                    }
                    let bytes_before = fs.read_text_file("/triple.jsonl", None).await.unwrap();
                    let expected = memory
                        .append_record_entry_and_record(
                            record(left, "before"),
                            entry("message"),
                            "main",
                            record(right, "after"),
                        )
                        .await;
                    let actual = jsonl
                        .append_record_entry_and_record(
                            record(left, "before"),
                            entry("message"),
                            "main",
                            record(right, "after"),
                        )
                        .await;
                    assert_eq!(
                        actual.is_ok(),
                        expected.is_ok(),
                        "seeded={seeded} before={left} after={right}"
                    );
                    if actual.is_ok() {
                        assert_eq!(
                            jsonl.get_log(&LogOptions::default()).await.unwrap(),
                            memory.get_log(&LogOptions::default()).await.unwrap()
                        );
                    } else {
                        assert_eq!(
                            fs.read_text_file("/triple.jsonl", None).await.unwrap(),
                            bytes_before,
                            "invalid triple changed file"
                        );
                    }
                }
            }
        }
    }
    #[tokio::test]
    async fn finishing_one_of_multiple_existing_operations_does_not_free_slot() {
        let (fs, _) = fixture().await;
        for (seq, id) in [(1, "older"), (2, "newer")] {
            let mut record = start(id, "main");
            if let LaneRecord::OperationStarted(operation) = &mut record {
                operation.base.seq = seq;
                operation.base.timestamp = seq as i64;
            }
            let encoded =
                rpi_harness::session::jsonl::encode_mutation(&SessionMutation::Record { record });
            fs.append_file("/triple.jsonl", rpi_tools::FileContent::Text(encoded), None)
                .await
                .unwrap();
        }
        let storage = JsonlSessionStorage::load(
            fs.clone(),
            "/triple.jsonl",
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .await
        .unwrap();
        let before = fs.read_text_file("/triple.jsonl", None).await.unwrap();
        for run in ["older", "newer"] {
            let result = storage
                .append_record_entry_and_record(
                    finish(&format!("finish-{run}"), run, "main"),
                    entry(&format!("message-{run}")),
                    "main",
                    start(&format!("new-{run}"), "main"),
                )
                .await;
            assert!(
                result.is_err(),
                "finishing {run} ignored the other existing operation"
            );
            assert_eq!(
                fs.read_text_file("/triple.jsonl", None).await.unwrap(),
                before
            );
        }
    }
}
