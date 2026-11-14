//! Fork selection must agree between metadata-only JSONL and eager backends.
use rpi_harness::error::SessionErrorCode;
use rpi_harness::session::jsonl::{JsonlSessionRepo, JsonlSessionRepoOptions};
use rpi_harness::session::types::*;
use rpi_harness::session::{
    DefaultIdGenerator, InMemorySessionRepo, SqliteSessionRepo, SystemClock,
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use std::sync::Arc;

fn options(id: &str) -> SessionCreateOptions {
    SessionCreateOptions {
        id: Some(id.into()),
        ..Default::default()
    }
}
fn model(id: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.into(),
        kind: ProvisionedKind::ModelChange {
            provider: "test".into(),
            model_id: "model".into(),
        },
    }
}
fn thinking(id: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.into(),
        kind: ProvisionedKind::ThinkingLevel {
            thinking_level: "high".into(),
        },
    }
}
fn custom(id: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.into(),
        kind: ProvisionedKind::Custom {
            custom_type: "note".into(),
            data: Some(serde_json::json!({"body": id})),
        },
    }
}
fn message(id: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.into(),
        kind: ProvisionedKind::Message {
            message: rpi_ai::UserMessage::new(id, 0).into(),
            terminate: None,
        },
    }
}
fn oldest() -> EntryQuery {
    EntryQuery {
        order: Some(EntryOrder::OldestFirst),
        ..Default::default()
    }
}
async fn check<R: SessionRepo>(repo: R) {
    let source = repo.create(&options("source")).await.unwrap();
    for entry in [
        model("root"),
        message("m1"),
        model("between"),
        message("m2"),
        model("switch"),
        thinking("thinking"),
        custom("custom-tip"),
    ] {
        source.append_entry(entry, "main").await.unwrap();
    }
    source.set_name(Some("named source")).await.unwrap();
    source.set_label("m2", Some("message label")).await.unwrap();
    source
        .set_label("custom-tip", Some("actual leaf"))
        .await
        .unwrap();
    source.create_lane("side", Some("m1")).await.unwrap();
    source
        .append_entry(message("side-message"), "side")
        .await
        .unwrap();
    let metadata = source.metadata();
    let original_entries = source.find_entries(&oldest()).await.unwrap();
    let original_log = source.get_log(&LogOptions::default()).await.unwrap();
    let original_lanes = source.get_lanes().await.unwrap();
    // JSONL fork reopens a cold source, exercising metadata selection and
    // releasing the source writer claim. No fork may mutate the source.
    drop(source);
    for (index, entry_id, position, expected) in [
        (
            0,
            None,
            None,
            vec![
                "root",
                "m1",
                "between",
                "m2",
                "switch",
                "thinking",
                "custom-tip",
            ],
        ),
        (
            1,
            None,
            Some(ForkPosition::At),
            vec![
                "root",
                "m1",
                "between",
                "m2",
                "switch",
                "thinking",
                "custom-tip",
            ],
        ),
        (
            2,
            None,
            Some(ForkPosition::Before),
            vec!["root", "m1", "between", "m2", "switch", "thinking"],
        ),
        (3, Some("m2"), None, vec!["root", "m1", "between"]),
        (4, Some("m1"), None, vec!["root"]),
        (
            5,
            Some("m2"),
            Some(ForkPosition::At),
            vec!["root", "m1", "between", "m2"],
        ),
        (
            6,
            Some("m2"),
            Some(ForkPosition::Before),
            vec!["root", "m1", "between"],
        ),
    ] {
        let child = repo
            .fork(
                &metadata,
                &options(&format!("child-{index}")),
                &ForkOptions::Branch {
                    entry_id: entry_id.map(str::to_owned),
                    position,
                },
            )
            .await
            .unwrap();
        let entries = child.find_entries(&oldest()).await.unwrap();
        assert_eq!(entries.iter().map(Entry::id).collect::<Vec<_>>(), expected);
        for copied in &entries {
            let mut original = original_entries
                .iter()
                .find(|entry| entry.id() == copied.id())
                .unwrap()
                .clone();
            original.base_mut().seq = copied.seq();
            assert_eq!(&original, copied, "only sequence may be restamped");
        }
        assert_eq!(
            child.get_lanes().await.unwrap(),
            vec![LanePointer {
                lane: "main".into(),
                leaf_id: expected.last().map(|id| (*id).into())
            }]
        );
        assert_eq!(
            child.get_name().await.unwrap().as_deref(),
            Some("named source")
        );
        assert_eq!(
            child.get_label("custom-tip").await.unwrap().as_deref(),
            expected.contains(&"custom-tip").then_some("actual leaf")
        );
        if expected.contains(&"m2") {
            assert_eq!(
                child.get_label("m2").await.unwrap().as_deref(),
                Some("message label")
            );
        }
    }
    for id in ["root", "switch", "thinking", "custom-tip"] {
        for (index, position) in [None, Some(ForkPosition::At), Some(ForkPosition::Before)]
            .into_iter()
            .enumerate()
        {
            let result = repo
                .fork(
                    &metadata,
                    &options(&format!("rejected-{id}-{index}")),
                    &ForkOptions::Branch {
                        entry_id: Some(id.into()),
                        position,
                    },
                )
                .await;
            assert_eq!(
                result
                    .err()
                    .expect("explicit nonmessage target must be rejected")
                    .code,
                SessionErrorCode::InvalidForkTarget
            );
        }
    }
    let tree = repo
        .fork(&metadata, &options("tree"), &ForkOptions::Tree)
        .await
        .unwrap();
    let tree_entries = tree.find_entries(&oldest()).await.unwrap();
    let mut expected_tree = original_entries.clone();
    for (index, entry) in expected_tree.iter_mut().enumerate() {
        entry.base_mut().seq = index as u64 + 1;
    }
    assert_eq!(tree_entries, expected_tree);
    assert_eq!(tree.get_lanes().await.unwrap(), original_lanes);
    drop(tree);
    let reopened = repo.open(&metadata).await.unwrap();
    assert_eq!(
        reopened.get_log(&LogOptions::default()).await.unwrap(),
        original_log
    );
    assert_eq!(reopened.get_lanes().await.unwrap(), original_lanes);
    drop(reopened);

    // With no message ancestor at all, each nonmessage kind is a valid
    // implicit actual leaf. Before cuts its sole root away, leaving empty main.
    for (kind, entry) in [
        ("model", model("only-state")),
        ("thinking", thinking("only-state")),
        ("custom", custom("only-state")),
    ] {
        let source = repo
            .create(&options(&format!("only-{kind}")))
            .await
            .unwrap();
        source.append_entry(entry, "main").await.unwrap();
        let metadata = source.metadata();
        drop(source);
        for (index, position) in [None, Some(ForkPosition::At), Some(ForkPosition::Before)]
            .into_iter()
            .enumerate()
        {
            let child = repo
                .fork(
                    &metadata,
                    &options(&format!("only-{kind}-child-{index}")),
                    &ForkOptions::Branch {
                        entry_id: None,
                        position,
                    },
                )
                .await
                .unwrap();
            let before = matches!(position, Some(ForkPosition::Before));
            assert_eq!(
                child.find_entries(&oldest()).await.unwrap().len(),
                usize::from(!before)
            );
            assert_eq!(
                child.get_lanes().await.unwrap(),
                vec![LanePointer {
                    lane: "main".into(),
                    leaf_id: (!before).then(|| "only-state".into())
                }]
            );
        }
    }
    // Explicit Before defaults to an empty cut when the target is a root message.
    let source = repo.create(&options("root-message")).await.unwrap();
    source
        .append_entry(message("root-message"), "main")
        .await
        .unwrap();
    let metadata = source.metadata();
    drop(source);
    let child = repo
        .fork(
            &metadata,
            &options("before-root"),
            &ForkOptions::Branch {
                entry_id: Some("root-message".into()),
                position: None,
            },
        )
        .await
        .unwrap();
    assert!(child.find_entries(&oldest()).await.unwrap().is_empty());
    assert_eq!(
        child.get_lanes().await.unwrap(),
        vec![LanePointer {
            lane: "main".into(),
            leaf_id: None
        }]
    );

    let empty = repo.create(&options("empty")).await.unwrap();
    let metadata = empty.metadata();
    drop(empty);
    let child = repo
        .fork(&metadata, &options("empty-child"), &ForkOptions::default())
        .await
        .unwrap();
    assert!(child.find_entries(&oldest()).await.unwrap().is_empty());
}

#[tokio::test]
async fn memory_fork_tip_contract() {
    check(InMemorySessionRepo::new(
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    ))
    .await;
}
#[tokio::test]
async fn sqlite_fork_tip_contract() {
    let dir = tempfile::tempdir().unwrap();
    check(SqliteSessionRepo::open(dir.path().join("sessions.db")).unwrap()).await;
}
#[tokio::test]
async fn jsonl_cold_fork_tip_contract() {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    check(JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs,
        sessions_root: dir.path().to_string_lossy().into_owned(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    }))
    .await;
}
