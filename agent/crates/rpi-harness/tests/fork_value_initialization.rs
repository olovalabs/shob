//! Host-provided new-identity values belong to the same metadata-first fork
//! publication, independently of the selected historical cut.
use rpi_harness::error::SessionError;
use rpi_harness::session::jsonl::types::JsonlSessionCreateOptions;
use rpi_harness::session::jsonl::{JsonlSessionRepo, JsonlSessionRepoOptions, JsonlSessionStorage};
use rpi_harness::session::types::*;
use rpi_harness::session::{DefaultIdGenerator, SessionValues, SystemClock};
use rpi_tools::env::FileContent;
use rpi_tools::{FileSystem, OsExecutionEnv};
use serde_json::{json, Value};
use std::sync::Arc;

const BINDING: &str = "rpi.resource-selection.v1";

fn options(id: &str, cwd: &str) -> JsonlSessionCreateOptions {
    JsonlSessionCreateOptions {
        id: Some(id.into()),
        parent_session_id: None,
        cwd: cwd.into(),
        metadata: None,
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
fn value(id: &str, key: &str, value: Value) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.into(),
        kind: ProvisionedKind::Custom {
            custom_type: "rpi.value".into(),
            data: Some(json!({"key": key, "value": value})),
        },
    }
}
async fn entries(storage: &JsonlSessionStorage) -> Vec<Entry> {
    storage
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn cold_fork_stages_validated_initial_binding_after_every_cut_including_tree() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_string_lossy().into_owned();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: fs.clone(),
        sessions_root: cwd.clone(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });
    let source = repo.create_typed(&options("source", &cwd)).await.unwrap();
    source.append_entry(message("m1"), "main").await.unwrap();
    source
        .append_entry(
            value(
                "old-binding",
                BINDING,
                json!({"revision": 4, "declaration": "old", "inventory": "old"}),
            ),
            "main",
        )
        .await
        .unwrap();
    source.append_entry(message("m2"), "main").await.unwrap();
    source
        .append_entry(
            ProvisionedEntry {
                id: "model".into(),
                kind: ProvisionedKind::ModelChange {
                    provider: "test".into(),
                    model_id: "model".into(),
                },
            },
            "main",
        )
        .await
        .unwrap();
    source
        .append_entry(
            ProvisionedEntry {
                id: "thinking".into(),
                kind: ProvisionedKind::ThinkingLevel {
                    thinking_level: "high".into(),
                },
            },
            "main",
        )
        .await
        .unwrap();
    source.create_lane("side", Some("m1")).await.unwrap();
    // Latest source-wide authority is outside the main branch and after m2.
    // The storage seam must pass it to the host even for Before(m1)'s empty cut.
    let latest = json!({"revision": 9, "declaration": "latest", "inventory": "latest", "lastOperation": {"operationId": "source-op"}, "retry": ["source-retry"]});
    source
        .append_entry(value("latest-binding", BINDING, latest.clone()), "side")
        .await
        .unwrap();
    let metadata = source.jsonl_metadata();
    let history = fs.read_binary_file(&metadata.path, None).await.unwrap();
    let progress_path = format!("{}.progress.jsonl", metadata.path);
    let progress = fs.read_binary_file(&progress_path, None).await.unwrap();
    drop(source);
    let initial = json!({"revision": 1, "declaration": "latest", "inventory": "latest"});
    for (index, fork, expected_parent, expected_count) in [
        (0, ForkOptions::default(), Some("thinking"), 5),
        (
            1,
            ForkOptions::Branch {
                entry_id: None,
                position: Some(ForkPosition::Before),
            },
            Some("model"),
            4,
        ),
        (
            2,
            ForkOptions::Branch {
                entry_id: Some("m1".into()),
                position: None,
            },
            None,
            0,
        ),
        (
            3,
            ForkOptions::Branch {
                entry_id: Some("m2".into()),
                position: Some(ForkPosition::At),
            },
            Some("m2"),
            3,
        ),
        (4, ForkOptions::Tree, Some("thinking"), 6),
    ] {
        let initialized = initial.clone();
        let child = repo
            .fork_typed_with_value_initializer(
                &metadata,
                &options(&format!("child-{index}"), &cwd),
                &fork,
                |values| {
                    assert_eq!(values.get(BINDING), Some(&latest));
                    // Validation/reset belongs to the CLI, not to the storage
                    // engine. Supply exactly the approved new identity payload.
                    Ok(vec![(BINDING.into(), initialized)])
                },
            )
            .await
            .unwrap();
        let child_metadata = child.jsonl_metadata();
        let copied = entries(&child).await;
        assert_eq!(copied.len(), expected_count + 1);
        let appended = copied.last().unwrap();
        assert_eq!(appended.parent_id(), expected_parent);
        assert!(!["old-binding", "latest-binding"].contains(&appended.id()));
        let Entry::Custom(binding) = appended else {
            panic!("initialization must be a custom value")
        };
        assert_eq!(binding.custom_type, "rpi.value");
        assert_eq!(
            binding.data,
            Some(json!({"key": BINDING, "value": initial}))
        );
        assert_eq!(
            SessionValues::from_entries(&copied).get(BINDING),
            Some(&initial)
        );
        let lanes = child.get_lanes().await.unwrap();
        assert_eq!(
            lanes
                .iter()
                .find(|lane| lane.lane == "main")
                .unwrap()
                .leaf_id
                .as_deref(),
            Some(appended.id())
        );
        if matches!(fork, ForkOptions::Tree) {
            assert_eq!(
                lanes
                    .iter()
                    .find(|lane| lane.lane == "side")
                    .unwrap()
                    .leaf_id
                    .as_deref(),
                Some("latest-binding")
            );
        }
        let log = child.get_log(&LogOptions::default()).await.unwrap();
        let LogItem::Entry { entry, .. } = log.last().unwrap() else {
            panic!("initial value must be the final staged mutation")
        };
        assert_eq!(entry.id(), appended.id());
        assert_eq!(child_metadata.parent_session_id.as_deref(), Some("source"));
        drop(child);
        let reopened = JsonlSessionStorage::load(
            fs.clone(),
            &child_metadata.path,
            Arc::new(SystemClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap();
        assert_eq!(entries(&reopened).await, copied);
        assert_eq!(reopened.get_log(&LogOptions::default()).await.unwrap(), log);
        assert_eq!(
            fs.read_binary_file(&metadata.path, None).await.unwrap(),
            history
        );
        assert_eq!(
            fs.read_binary_file(&progress_path, None).await.unwrap(),
            progress
        );
    }
}

#[tokio::test]
async fn cold_custom_only_without_authority_and_rejected_initialization_do_not_mutate_source() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_string_lossy().into_owned();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: fs.clone(),
        sessions_root: cwd.clone(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });
    let source = repo.create_typed(&options("source", &cwd)).await.unwrap();
    source
        .append_entry(value("ordinary-value", "ordinary", json!("kept")), "main")
        .await
        .unwrap();
    let metadata = source.jsonl_metadata();
    drop(source);
    // A recoverable torn source tail is ignored in the read-only snapshot,
    // never repaired as a side effect of forking.
    fs.append_file(&metadata.path, FileContent::Text("{\"type\":".into()), None)
        .await
        .unwrap();
    let history = fs.read_binary_file(&metadata.path, None).await.unwrap();
    let child = repo
        .fork_typed_with_value_initializer(
            &metadata,
            &options("no-authority", &cwd),
            &ForkOptions::default(),
            |values| {
                assert!(values.get(BINDING).is_none());
                assert_eq!(values.get("ordinary"), Some(&json!("kept")));
                Ok(Vec::new())
            },
        )
        .await
        .unwrap();
    let copied = entries(&child).await;
    assert_eq!(copied.len(), 1);
    assert_eq!(copied[0].id(), "ordinary-value");
    assert!(SessionValues::from_entries(&copied).get(BINDING).is_none());
    drop(child);
    for (id, invalid_key) in [("rejected-host", false), ("rejected-key", true)] {
        let result = repo
            .fork_typed_with_value_initializer(
                &metadata,
                &options(id, &cwd),
                &ForkOptions::default(),
                |_| {
                    if invalid_key {
                        Ok(vec![(" ".into(), json!(1))])
                    } else {
                        Err(SessionError::invalid_payload("host validation failed"))
                    }
                },
            )
            .await;
        assert!(result.is_err());
        assert!(!repo
            .list_typed(
                &rpi_harness::session::jsonl::types::JsonlSessionListOptions {
                    cwd: Some(cwd.clone())
                }
            )
            .await
            .unwrap()
            .iter()
            .any(|metadata| metadata.id == id));
    }
    assert_eq!(
        fs.read_binary_file(&metadata.path, None).await.unwrap(),
        history
    );
}
