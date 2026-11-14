//! Outcome success must not erase a queued storage failure from an earlier
//! write. Uses the real harness queue and native writer, not a fabricated flag.
use super::*;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_harness::{
    session::{experimental_native::ExperimentalNativeRepo, types::*},
    types::AgentHarnessOptions,
};
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_print_and_json_keep_queued_write_failure_sticky_across_successful_prompts() {
    for json_mode in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let repo = ExperimentalNativeRepo::new(temp.path().to_owned())
            .await
            .unwrap();
        let storage = repo.create(&SessionCreateOptions::default()).await.unwrap();
        let id = storage.metadata().id.clone();
        let provider = FauxProvider::new(
            FauxScript::new()
                .with_text("first successful outcome")
                .with_text("second successful outcome"),
        );
        let model = provider.default_model().clone();
        let harness = AgentHarness::create(AgentHarnessOptions {
            session: rpi_harness::session::Session::new(Arc::new(storage), None),
            model,
            models: vec![provider],
            ..Default::default()
        })
        .await
        .unwrap();
        let db = rusqlite::Connection::open(temp.path().join(format!("{id}.rpinative"))).unwrap();
        db.execute_batch("CREATE TRIGGER seed_failure BEFORE INSERT ON facts BEGIN SELECT RAISE(ABORT,'seeded native write failure');END;").unwrap();
        assert!(harness
            .append_message(rpi_ai::UserMessage::new("refused out-of-band", 42).into())
            .await
            .is_err());
        db.execute_batch("DROP TRIGGER seed_failure;").unwrap();
        drop(db);
        let args = Args {
            print: true,
            experimental_native_store: Some(temp.path().to_owned()),
            ..Default::default()
        };
        let code = if json_mode {
            json(
                &harness,
                &args,
                Some("first prompt".into()),
                &["second prompt".into()],
                vec![],
                None,
            )
            .await
        } else {
            print(
                &harness,
                &args,
                Some("first prompt".into()),
                &["second prompt".into()],
                vec![],
            )
            .await
        };
        assert_eq!(
            code, 1,
            "later successful outcomes hid a queued write failure"
        );
        assert!(
            harness.take_write_failures().is_empty(),
            "mode did not consume the actual failure queue"
        );
        let entries = harness
            .session()
            .find_entries(&EntryQuery {
                entry_type: Some("message"),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(entries.len(), 4, "both prompts must actually complete");
    }
}
#[test]
fn native_failure_queue_json_wire_has_one_final_failed_result_after_successful_runs() {
    let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","modes::native_persistence_tests::native_print_and_json_keep_queued_write_failure_sticky_across_successful_prompts","--nocapture","--test-threads=1"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("session not saved"));
    let events = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect::<Vec<_>>();
    assert_eq!(
        events
            .iter()
            .filter(|e| e["type"] == "run_end" && e["outcome"] == "completed")
            .count(),
        2
    );
    assert!(events
        .iter()
        .any(|e| e["type"] == "error" && e["kind"] == "session_write_failed"));
    assert_eq!(events.iter().filter(|e| e["type"] == "result").count(), 1);
    let result = events.last().unwrap();
    assert_eq!(result["type"], "result");
    assert_eq!(result["outcome"], "failed");
    assert_eq!(result["persistenceFailed"], true);
}
