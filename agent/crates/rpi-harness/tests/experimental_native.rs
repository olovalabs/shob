#![cfg(feature = "experimental-native-store")]
use rpi_harness::{
    error::SessionErrorCode,
    session::{
        experimental_native::ExperimentalNativeRepo,
        memory::{Clock, CounterIdGenerator},
        types::*,
        InMemorySessionStorage, Session,
    },
};
use serde_json::json;
use std::sync::Arc;
struct FixedClock;
impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        42
    }
}
fn entry(id: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.into(),
        kind: ProvisionedKind::Message {
            message: rpi_ai::UserMessage::new(format!("body-{id}"), 42).into(),
            terminate: None,
        },
    }
}
fn record(id: &str, kind: &str) -> LaneRecord {
    serde_json::from_value(json!({"id":id,"seq":0,"timestamp":0,"lane":"main","type":kind,"runId":"run","entryId":"queued","outcome":"completed"})).unwrap()
}
fn queue(id: &str, target: &str, kind: &str) -> LaneRecord {
    serde_json::from_value(json!({"id":id,"seq":0,"timestamp":0,"lane":"main","type":"queue_enqueued","queue":kind,"target":{"id":target,"type":"message","message":{"kind":"user","role":"user","content":"body","timestamp":42}}})).unwrap()
}
fn start(id: &str) -> LaneRecord {
    serde_json::from_value(json!({"id":id,"seq":0,"timestamp":0,"lane":"main","type":"operation_started","sourceLeafId":null,"intent":{"kind":"run","originalPrompt":[],"initialMessages":[]}})).unwrap()
}
async fn fixture() -> (
    tempfile::TempDir,
    ExperimentalNativeRepo,
    rpi_harness::session::experimental_native::ExperimentalNativeStorage,
) {
    let d = tempfile::tempdir().unwrap();
    let repo = ExperimentalNativeRepo::with_clock(d.path().to_owned(), Arc::new(FixedClock))
        .await
        .unwrap();
    let s = repo
        .create(&SessionCreateOptions {
            id: Some("fixture".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    (d, repo, s)
}
#[tokio::test]
async fn trait_queries_match_memory_and_reopen() {
    let (_d, repo, s) = fixture().await;
    let memory = InMemorySessionStorage::new(
        s.metadata(),
        Arc::new(FixedClock),
        Arc::new(CounterIdGenerator::new()),
    );
    for backend in [&s as &dyn SessionStorage, &memory as &dyn SessionStorage] {
        backend.append_entry(entry("first"), "main").await.unwrap();
        backend.create_lane("side", Some("first")).await.unwrap();
        backend
            .append_entry(entry("side-entry"), "side")
            .await
            .unwrap();
        backend.append_entry(entry("second"), "main").await.unwrap();
        backend.append_record(start("run")).await.unwrap();
        backend
            .append_records(vec![
                queue("q1", "queued", "steer"),
                queue("q2", "future", "followUp"),
            ])
            .await
            .unwrap();
        backend.set_name(Some("name")).await.unwrap();
        backend.set_label("first", Some("label")).await.unwrap();
    }
    assert_eq!(
        s.get_lanes().await.unwrap(),
        memory.get_lanes().await.unwrap()
    );
    for q in [
        EntryQuery::default(),
        EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            limit: Some(1),
            ..Default::default()
        },
        EntryQuery {
            entry_id: Some("side-entry".into()),
            ..Default::default()
        },
        EntryQuery {
            message_boundary_only: true,
            cursor: Some(EntryCursor { after_seq: 2 }),
            ..Default::default()
        },
    ] {
        assert_eq!(
            s.find_entries(&q).await.unwrap(),
            memory.find_entries(&q).await.unwrap()
        );
    }
    for q in [
        RecordQuery::default(),
        RecordQuery {
            lane: Some("main".into()),
            record_type: Some("queue_enqueued"),
            pending_queue_only: true,
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        },
        RecordQuery {
            queue_kind: Some(QueueKind::FollowUp),
            limit: Some(1),
            ..Default::default()
        },
        RecordQuery {
            record_type: Some("operation_started"),
            operation_kind: Some("run"),
            run_id: Some("run".into()),
            ..Default::default()
        },
    ] {
        assert_eq!(
            s.find_records(&q).await.unwrap(),
            memory.find_records(&q).await.unwrap()
        );
    }
    assert_eq!(
        s.find_entries_on_branch(
            &EntryQuery::default(),
            &BranchBounds::default(),
            "side-entry"
        )
        .await
        .unwrap(),
        memory
            .find_entries_on_branch(
                &EntryQuery::default(),
                &BranchBounds::default(),
                "side-entry"
            )
            .await
            .unwrap()
    );
    assert_eq!(
        s.get_stats().await.unwrap(),
        memory.get_stats().await.unwrap()
    );
    assert_eq!(s.get_name().await.unwrap(), Some("name".into()));
    assert_eq!(s.get_label("first").await.unwrap(), Some("label".into()));
    let page = RecordPageQuery {
        lane: "main".into(),
        after_seq: None,
        before_seq: None,
        limit: 2,
        from_end: true,
    };
    let a = s.record_page(&page).await.unwrap();
    let b = memory.record_page(&page).await.unwrap();
    assert_eq!(a.records, b.records);
    assert_eq!(a.total, b.total);
    let metadata = s.metadata();
    drop(s);
    let reopened = repo.open(&metadata).await.unwrap();
    assert_eq!(
        reopened.history_revision().await.unwrap(),
        memory.history_revision().await.unwrap()
    );
    let facade = Session::new(Arc::new(reopened), None);
    assert_eq!(
        facade.view("main").get_leaf_id().await.unwrap(),
        Some("second".into())
    );
}
#[tokio::test]
async fn atomic_batches_conditional_writes_and_cancellations() {
    let (_d, _repo, s) = fixture().await;
    let before = s.history_revision().await.unwrap();
    assert!(s
        .append_record_entry_and_record(
            record("before", "abort_requested"),
            entry("entry"),
            "main",
            record("before", "operation_finished")
        )
        .await
        .is_err());
    assert_eq!(s.history_revision().await.unwrap(), before);
    assert!(s.get_entry("entry").await.unwrap().is_none());
    assert!(s.get_log(&LogOptions::default()).await.unwrap().is_empty());
    let (a, e, b) = s
        .append_record_entry_and_record(
            record("before", "abort_requested"),
            entry("entry"),
            "main",
            record("after", "operation_finished"),
        )
        .await
        .unwrap();
    assert_eq!((a.seq(), e.seq(), b.seq()), (1, 2, 3));
    assert!(s
        .append_entry_if_revision(entry("stale"), "main", 0)
        .await
        .is_err());
    assert_eq!(s.history_revision().await.unwrap(), 3);
    s.append_entry_if_revision(entry("next"), "main", 3)
        .await
        .unwrap();
    assert!(s
        .append_queue_cancellations(vec![record("not-cancel", "abort_requested")])
        .await
        .is_err());
    let cancel = record("cancel", "queue_cancelled");
    assert!(s
        .append_queue_cancellations(vec![cancel.clone(), cancel])
        .await
        .is_err());
    assert_eq!(s.history_revision().await.unwrap(), 4);
}
#[tokio::test]
async fn transient_progress_has_independent_sequence_and_terminal_proof() {
    let (_d, repo, s) = fixture().await;
    let frame = AssistantFrameRecord {
        base: RecordBase {
            id: "frame".into(),
            seq: 0,
            lane: "main".into(),
            timestamp: 0,
        },
        run_id: "run".into(),
        stream_index: 0,
        op: AssistantFrameOp::Append,
        frame: Some(json!({"delta":"opaque"})),
    };
    assert_eq!(
        s.append_progress(vec![frame.clone()]).await.unwrap()[0]
            .base
            .seq,
        1
    );
    assert_eq!(s.history_revision().await.unwrap(), 0);
    let mut binding = frame.clone();
    binding.base.id = "binding".into();
    binding.op = AssistantFrameOp::CommitStream;
    binding.frame = Some(json!({"entryId":"assistant"}));
    assert_eq!(
        s.append_progress(vec![binding]).await.unwrap_err().code,
        SessionErrorCode::InvalidPayload
    );
    assert_eq!(s.prune_completed_progress().await.unwrap(), 0);
    s.create_lane("side", None).await.unwrap();
    let mut terminal = record("side-finish", "operation_finished");
    if let LaneRecord::OperationFinished(ref mut r) = terminal {
        r.base.lane = "side".into();
    }
    s.append_record(terminal).await.unwrap();
    assert_eq!(s.prune_completed_progress().await.unwrap(), 0);
    s.append_record(record("finish", "operation_finished"))
        .await
        .unwrap();
    let rev = s.history_revision().await.unwrap();
    assert_eq!(s.prune_completed_progress().await.unwrap(), 1);
    assert_eq!(s.history_revision().await.unwrap(), rev);
    assert!(s.read_progress("run").await.unwrap().is_empty());
    let metadata = s.metadata();
    drop(s);
    let reopened = repo.open(&metadata).await.unwrap();
    assert!(reopened.read_progress("run").await.unwrap().is_empty());
}
#[tokio::test]
async fn explicit_repo_lifecycle_and_fork_are_separate_from_legacy() {
    let (d, repo, s) = fixture().await;
    s.append_entry(entry("first"), "main").await.unwrap();
    s.set_label("first", Some("label")).await.unwrap();
    std::fs::write(d.path().join("legacy.jsonl"), b"untouched legacy").unwrap();
    assert_eq!(repo.list().await.unwrap().len(), 1);
    assert!(repo
        .create(&SessionCreateOptions {
            id: Some("fixture".into()),
            ..Default::default()
        })
        .await
        .is_err());
    assert!(repo
        .create(&SessionCreateOptions {
            id: Some("../escape".into()),
            ..Default::default()
        })
        .await
        .is_err());
    assert!(repo.delete(&s.metadata()).await.is_err());
    let fork = repo
        .fork(
            &s.metadata(),
            &SessionCreateOptions {
                id: Some("fork".into()),
                ..Default::default()
            },
            &ForkOptions::Tree,
        )
        .await
        .unwrap();
    assert_eq!(
        fork.get_lanes().await.unwrap()[0].leaf_id,
        Some("first".into())
    );
    assert_eq!(
        fork.get_entry("first").await.unwrap().unwrap().id(),
        "first"
    );
    assert_eq!(fork.get_label("first").await.unwrap(), Some("label".into()));
    assert!(fork.read_progress("run").await.unwrap().is_empty());
    assert_eq!(
        std::fs::read(d.path().join("legacy.jsonl")).unwrap(),
        b"untouched legacy"
    );
}

#[tokio::test]
async fn independent_repo_handles_allocate_once_and_reject_stale_revision() {
    let (d, repo, s) = fixture().await;
    let other_repo = ExperimentalNativeRepo::with_clock(d.path().to_owned(), Arc::new(FixedClock))
        .await
        .unwrap();
    let other = other_repo.open(&s.metadata()).await.unwrap();
    let metadata = s.metadata();
    let mut tasks = Vec::new();
    for n in 0..24 {
        let handle = if n % 2 == 0 { s.clone() } else { other.clone() };
        tasks.push(tokio::spawn(async move {
            handle
                .append_entry_if_revision(entry(&format!("conditional-{n}")), "main", 0)
                .await
        }));
    }
    let mut successes = 0;
    for task in tasks {
        if task.await.unwrap().is_ok() {
            successes += 1;
        }
    }
    assert_eq!(successes, 1);
    assert_eq!(s.history_revision().await.unwrap(), 1);
    let mut tasks = Vec::new();
    for n in 0..40 {
        let handle = if n % 2 == 0 { s.clone() } else { other.clone() };
        tasks.push(tokio::spawn(async move {
            handle
                .append_entry(entry(&format!("parallel-{n}")), "main")
                .await
                .unwrap()
                .seq()
        }));
    }
    let mut sequences = Vec::new();
    for task in tasks {
        sequences.push(task.await.unwrap());
    }
    sequences.sort();
    assert_eq!(sequences, (2..=41).collect::<Vec<_>>());
    drop(s);
    drop(other);
    let reopened = repo.open(&metadata).await.unwrap();
    assert_eq!(reopened.history_revision().await.unwrap(), 41);
    assert_eq!(
        reopened
            .find_entries(&EntryQuery::default())
            .await
            .unwrap()
            .len(),
        41
    );
}
#[tokio::test]
async fn invalid_queries_fail_closed_and_empty_summaries_are_supported() {
    let (_d, _repo, s) = fixture().await;
    assert_eq!(
        s.find_entries(&EntryQuery {
            limit: Some(0),
            ..Default::default()
        })
        .await
        .unwrap_err()
        .code,
        SessionErrorCode::InvalidQuery
    );
    assert!(s
        .find_records(&RecordQuery {
            operation_kind: Some("run"),
            ..Default::default()
        })
        .await
        .unwrap()
        .is_empty());
    assert!(s
        .message_summaries("main", &MessageSummaryQuery::default())
        .await
        .unwrap()
        .messages
        .is_empty());
    assert_eq!(
        s.message_window("main", &MessageWindowQuery::default())
            .await
            .unwrap_err()
            .code,
        SessionErrorCode::InvalidQuery
    );
}

#[tokio::test]
async fn cancellation_batch_requires_matching_pending_run_and_rolls_back_whole_batch() {
    let (_d, _repo, s) = fixture().await;
    let a:LaneRecord=serde_json::from_value(json!({"id":"qa","seq":0,"timestamp":0,"lane":"main","type":"queue_enqueued","queue":"steer","runId":"run","target":{"id":"queued"}})).unwrap();
    let b:LaneRecord=serde_json::from_value(json!({"id":"qb","seq":0,"timestamp":0,"lane":"main","type":"queue_enqueued","queue":"steer","runId":"other-run","target":{"id":"other"}})).unwrap();
    s.append_records(vec![a, b]).await.unwrap();
    let valid = record("cancel", "queue_cancelled");
    let mut invalid = record("invalid", "queue_cancelled");
    if let LaneRecord::QueueCancelled(r) = &mut invalid {
        r.entry_id = "other".into();
    }
    assert!(s
        .append_queue_cancellations(vec![valid.clone(), invalid])
        .await
        .is_err());
    assert_eq!(s.history_revision().await.unwrap(), 2);
    assert_eq!(
        s.find_records(&RecordQuery {
            pending_queue_only: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .len(),
        2
    );
    s.append_queue_cancellations(vec![valid]).await.unwrap();
    assert_eq!(s.history_revision().await.unwrap(), 3);
}

#[path = "support/experimental_pages.rs"]
mod page_matrix;
#[path = "support/experimental_queries.rs"]
mod query_matrix;
