//! Cancellation ACK and borrowed pending-query contracts shared by all built-in stores.
use rpi_harness::session::jsonl::{JsonlSessionStorage, JsonlV4Header};
use rpi_harness::session::types::*;
use rpi_harness::session::{
    DefaultIdGenerator, InMemorySessionStorage, SqliteSessionRepo, SystemClock,
};
use std::sync::Arc;

fn base(id: &str) -> RecordBase {
    RecordBase {
        id: id.into(),
        lane: "main".into(),
        seq: 0,
        timestamp: 0,
    }
}
fn enqueue(id: &str, kind: QueueKind) -> LaneRecord {
    LaneRecord::QueueEnqueued(QueueEnqueuedRecord {
        base: base(&format!("queue-{id}")),
        queue: kind,
        run_id: None,
        target: serde_json::to_value(ProvisionedEntry {
            id: id.into(),
            kind: ProvisionedKind::Message {
                message: rpi_ai::UserMessage::new(id, 0).into(),
                terminate: None,
            },
        })
        .unwrap(),
    })
}
fn cancel(id: &str, target: &str) -> LaneRecord {
    LaneRecord::QueueCancelled(QueueCancelledRecord {
        base: base(id),
        run_id: None,
        entry_id: target.into(),
    })
}
async fn stores() -> (tempfile::TempDir, Vec<Arc<dyn SessionStorage>>) {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(SystemClock);
    let ids = Arc::new(DefaultIdGenerator::new());
    let memory = InMemorySessionStorage::new(
        SessionMetadata {
            id: "memory".into(),
            created_at: 0,
            parent_session_id: None,
        },
        clock.clone(),
        ids.clone(),
    );
    let repo = SqliteSessionRepo::open(dir.path().join("sessions.db")).unwrap();
    let sqlite = repo
        .create(&SessionCreateOptions {
            id: Some("sqlite".into()),
            parent_session_id: None,
            metadata: None,
        })
        .await
        .unwrap();
    let jsonl = JsonlSessionStorage::create(
        Arc::new(rpi_tools::OsExecutionEnv::with_cwd(dir.path().into())),
        &dir.path().join("session.jsonl").to_string_lossy(),
        JsonlV4Header::new(
            "jsonl".into(),
            1,
            dir.path().display().to_string(),
            None,
            None,
            None,
        ),
        clock,
        ids,
    )
    .await
    .unwrap();
    (
        dir,
        vec![Arc::new(memory), Arc::new(sqlite), Arc::new(jsonl)],
    )
}
#[tokio::test]
async fn cancellation_batches_validate_all_items_before_publishing_on_every_backend() {
    let (_dir, stores) = stores().await;
    for store in stores {
        store
            .append_record(enqueue("first", QueueKind::Steer))
            .await
            .unwrap();
        store
            .append_record(enqueue("second", QueueKind::FollowUp))
            .await
            .unwrap();
        let revision = store.history_revision().await.unwrap();
        let mut wrong_lane = cancel("wrong-lane", "second");
        if let LaneRecord::QueueCancelled(record) = &mut wrong_lane {
            record.base.lane = "missing".into();
        }
        for batch in [
            vec![
                cancel("cancel-first", "first"),
                cancel("unknown", "unknown"),
            ],
            vec![cancel("cancel-first", "first"), wrong_lane],
            vec![
                cancel("cancel-first", "first"),
                cancel("same-target", "first"),
            ],
            vec![cancel("same-id", "first"), cancel("same-id", "second")],
        ] {
            assert!(store.append_queue_cancellations(batch).await.is_err());
            assert_eq!(store.history_revision().await.unwrap(), revision);
            assert_eq!(
                store
                    .find_records(&RecordQuery {
                        record_type: Some("queue_enqueued"),
                        pending_queue_only: true,
                        ..Default::default()
                    })
                    .await
                    .unwrap()
                    .len(),
                2
            );
        }
        store
            .append_queue_cancellations(vec![
                cancel("cancel-first", "first"),
                cancel("cancel-second", "second"),
            ])
            .await
            .unwrap();
        assert!(store
            .find_records(&RecordQuery {
                record_type: Some("queue_enqueued"),
                pending_queue_only: true,
                ..Default::default()
            })
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .append_queue_cancellations(vec![cancel("again", "first")])
            .await
            .is_err());
    }
}
#[tokio::test]
async fn pending_queries_filter_before_limits_and_exclude_committed_targets_on_every_backend() {
    let (_dir, stores) = stores().await;
    for store in stores {
        store
            .append_record(enqueue("cancelled", QueueKind::Context))
            .await
            .unwrap();
        store
            .append_queue_cancellations(vec![cancel("cancel", "cancelled")])
            .await
            .unwrap();
        store
            .append_record(enqueue("committed", QueueKind::Context))
            .await
            .unwrap();
        store
            .append_entry(
                ProvisionedEntry {
                    id: "committed".into(),
                    kind: ProvisionedKind::Message {
                        message: rpi_ai::UserMessage::new("already committed", 0).into(),
                        terminate: None,
                    },
                },
                "main",
            )
            .await
            .unwrap();
        store
            .append_record(enqueue("other-kind", QueueKind::Steer))
            .await
            .unwrap();
        store
            .append_record(enqueue("pending", QueueKind::Context))
            .await
            .unwrap();
        let query = RecordQuery {
            lane: Some("main".into()),
            record_type: Some("queue_enqueued"),
            queue_kind: Some(QueueKind::Context),
            pending_queue_only: true,
            limit: Some(1),
            ..Default::default()
        };
        let pending = store.find_records(&query).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id(), "queue-pending");
        assert!(store
            .find_records(&RecordQuery {
                target_entry_id: Some("cancelled".into()),
                ..query
            })
            .await
            .unwrap()
            .is_empty());
    }
}
