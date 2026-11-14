//! Experimental selectors must exclude archives before limits/body decoding.
#![cfg(feature = "experimental-native-store")]

use std::sync::Arc;

use rpi_harness::{
    session::{
        context::build_session_context,
        experimental_native::{ExperimentalNativeRepo, ExperimentalNativeStorage},
        memory::{Clock, CounterIdGenerator},
        types::*,
        InMemorySessionStorage,
    },
    tool_program_body::{CHUNK_CUSTOM_TYPE, COMMIT_CUSTOM_TYPE},
};
use serde_json::json;

struct FixedClock;
impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        42
    }
}

async fn fixture() -> (
    tempfile::TempDir,
    ExperimentalNativeRepo,
    ExperimentalNativeStorage,
) {
    let dir = tempfile::tempdir().unwrap();
    let repo = ExperimentalNativeRepo::with_clock(dir.path().into(), Arc::new(FixedClock))
        .await
        .unwrap();
    let storage = repo
        .create(&SessionCreateOptions {
            id: Some("body-experimental".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    (dir, repo, storage)
}

fn custom(kind: &str) -> ProvisionedKind {
    ProvisionedKind::Custom {
        custom_type: kind.into(),
        data: Some(json!({"payload": "x".repeat(48 * 1024)})),
    }
}
fn user(id: &str) -> ProvisionedKind {
    ProvisionedKind::Message {
        message: rpi_ai::UserMessage::new(id, 42).into(),
        terminate: None,
    }
}
async fn populate(storage: &dyn SessionStorage) {
    for (id, kind) in [
        ("old-chunk", custom(CHUNK_CUSTOM_TYPE)),
        (
            "model",
            ProvisionedKind::ModelChange {
                provider: "test".into(),
                model_id: "model".into(),
            },
        ),
        (
            "thinking",
            ProvisionedKind::ThinkingLevel {
                thinking_level: "high".into(),
            },
        ),
        (
            "tools",
            ProvisionedKind::ActiveTools {
                active_tool_names: vec!["test-tool".into()],
            },
        ),
        ("before", user("before")),
        (
            "cut",
            ProvisionedKind::Compaction {
                summary: "summary".into(),
                retained_tail: vec![],
                tokens_before: 42,
                details: None,
                usage: None,
            },
        ),
        ("after", user("after")),
        ("extension", custom("extension")),
        ("near-chunk", custom("rpi.tool_program.body_chunk.v1.extra")),
        (
            "near-commit",
            custom("rpi.tool_program.body_commit.v1.extra"),
        ),
        ("chunk", custom(CHUNK_CUSTOM_TYPE)),
        ("commit", custom(COMMIT_CUSTOM_TYPE)),
    ] {
        storage
            .append_entry(
                ProvisionedEntry {
                    id: id.into(),
                    kind,
                },
                "main",
            )
            .await
            .unwrap();
    }
    storage.create_lane("side", Some("before")).await.unwrap();
    for (id, kind) in [
        ("side-user", user("side-user")),
        ("side-chunk", custom(CHUNK_CUSTOM_TYPE)),
        ("side-commit", custom(COMMIT_CUSTOM_TYPE)),
    ] {
        storage
            .append_entry(
                ProvisionedEntry {
                    id: id.into(),
                    kind,
                },
                "side",
            )
            .await
            .unwrap();
    }
}
fn ids(entries: &[Entry]) -> Vec<&str> {
    entries.iter().map(Entry::id).collect()
}
fn filtered() -> EntryQuery {
    EntryQuery {
        exclude_tool_program_bodies: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn global_and_branch_queries_match_memory_before_limits() {
    let (_dir, _repo, storage) = fixture().await;
    let memory = InMemorySessionStorage::new(
        storage.metadata(),
        Arc::new(FixedClock),
        Arc::new(CounterIdGenerator::new()),
    );
    populate(&storage).await;
    populate(&memory).await;
    assert!(!EntryQuery::default().exclude_tool_program_bodies);
    let mut queries = vec![EntryQuery::default(), filtered()];
    for order in [EntryOrder::OldestFirst, EntryOrder::NewestFirst] {
        queries.push(EntryQuery {
            order: Some(order),
            limit: Some(1),
            ..filtered()
        });
        queries.push(EntryQuery {
            order: Some(order),
            limit: Some(1),
            cursor: Some(EntryCursor { after_seq: 7 }),
            ..filtered()
        });
    }
    for kind in [
        CHUNK_CUSTOM_TYPE,
        COMMIT_CUSTOM_TYPE,
        "extension",
        "rpi.tool_program.body_chunk.v1.extra",
        "rpi.tool_program.body_commit.v1.extra",
    ] {
        for exclude in [false, true] {
            queries.push(EntryQuery {
                custom_type: Some(kind.into()),
                exclude_tool_program_bodies: exclude,
                ..Default::default()
            });
        }
    }
    for entry_type in ["message", "custom", "compaction", "model_change"] {
        queries.push(EntryQuery {
            entry_type: Some(entry_type),
            ..filtered()
        });
    }
    for id in ["chunk", "commit", "after", "near-commit"] {
        queries.push(EntryQuery {
            entry_id: Some(id.into()),
            ..filtered()
        });
    }
    for q in queries {
        assert_eq!(
            storage.find_entries(&q).await.unwrap(),
            memory.find_entries(&q).await.unwrap()
        );
        for tip in ["commit", "side-commit"] {
            assert_eq!(
                storage
                    .find_entries_on_branch(&q, &BranchBounds::default(), tip)
                    .await
                    .unwrap(),
                memory
                    .find_entries_on_branch(&q, &BranchBounds::default(), tip)
                    .await
                    .unwrap(),
                "branch {tip}, query {q:?}"
            );
        }
    }
    assert_eq!(
        storage
            .find_entries(&EntryQuery::default())
            .await
            .unwrap()
            .len(),
        15
    );
    assert_eq!(storage.find_entries(&filtered()).await.unwrap().len(), 10);
    for (order, expected) in [
        (EntryOrder::OldestFirst, "model"),
        (EntryOrder::NewestFirst, "near-commit"),
    ] {
        let q = EntryQuery {
            order: Some(order),
            limit: Some(1),
            ..filtered()
        };
        assert_eq!(
            ids(&storage
                .find_entries_on_branch(&q, &BranchBounds::default(), "commit")
                .await
                .unwrap()),
            [expected]
        );
    }
    for kind in [CHUNK_CUSTOM_TYPE, COMMIT_CUSTOM_TYPE] {
        let q = EntryQuery {
            custom_type: Some(kind.into()),
            ..Default::default()
        };
        assert!(!storage.find_entries(&q).await.unwrap().is_empty());
        assert!(storage
            .find_entries(&EntryQuery {
                exclude_tool_program_bodies: true,
                ..q
            })
            .await
            .unwrap()
            .is_empty());
    }
}

#[tokio::test]
async fn default_context_preserves_compaction_and_state_facts() {
    let (_dir, _repo, storage) = fixture().await;
    populate(&storage).await;
    for tip in ["commit", "side-commit"] {
        let all = storage
            .find_entries_on_branch(
                &EntryQuery {
                    order: Some(EntryOrder::OldestFirst),
                    ..Default::default()
                },
                &BranchBounds::default(),
                tip,
            )
            .await
            .unwrap();
        let expected = build_session_context(&all, &Default::default());
        let actual = storage
            .context_on_branch(&BranchBounds::default(), tip)
            .await
            .unwrap();
        assert_eq!(actual.messages, expected.messages);
        assert_eq!(actual.model, expected.model);
        assert_eq!(actual.thinking_level, expected.thinking_level);
        assert_eq!(actual.active_tool_names, expected.active_tool_names);
        assert!(actual.model.is_some());
        assert_eq!(actual.thinking_level, "high");
        assert_eq!(actual.active_tool_names, Some(vec!["test-tool".into()]));
        assert_eq!(actual.messages.len(), 2);
    }
}

#[tokio::test]
async fn cold_corrupt_archives_are_skipped_but_explicit_reads_still_fail() {
    let (dir, repo, storage) = fixture().await;
    populate(&storage).await;
    let metadata = storage.metadata();
    drop(storage);
    // Reopen cold, then damage only bodies, preserving metadata/digests. Any
    // archive hydration must now fail rather than merely being projected away.
    let storage = repo.open(&metadata).await.unwrap();
    let db = rusqlite::Connection::open(dir.path().join("body-experimental.rpinative")).unwrap();
    assert_eq!(db.execute(
        "UPDATE facts SET body='poisoned archive body' WHERE seq IN (SELECT seq FROM selection WHERE json_extract(keys,'$.custom_type') IN (?1,?2))",
        [CHUNK_CUSTOM_TYPE, COMMIT_CUSTOM_TYPE],
    ).unwrap(), 5);
    drop(db);
    assert_eq!(storage.find_entries(&filtered()).await.unwrap().len(), 10);
    for tip in ["commit", "side-commit"] {
        let semantic = storage
            .find_entries_on_branch(&filtered(), &BranchBounds::default(), tip)
            .await
            .unwrap();
        assert!(!semantic.is_empty());
        let context = storage
            .context_on_branch(&BranchBounds::default(), tip)
            .await
            .unwrap();
        assert_eq!(context.messages.len(), 2);
        for order in [EntryOrder::OldestFirst, EntryOrder::NewestFirst] {
            let q = EntryQuery {
                order: Some(order),
                limit: Some(1),
                ..filtered()
            };
            assert_eq!(storage.find_entries(&q).await.unwrap().len(), 1);
            assert_eq!(
                storage
                    .find_entries_on_branch(&q, &BranchBounds::default(), tip)
                    .await
                    .unwrap()
                    .len(),
                1
            );
        }
        let error = storage
            .find_entries_on_branch(&EntryQuery::default(), &BranchBounds::default(), tip)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("native body digest mismatch"),
            "{error}"
        );
    }
    let error = storage
        .find_entries(&EntryQuery::default())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("native body digest mismatch"),
        "{error}"
    );
    for (id, kind) in [("chunk", CHUNK_CUSTOM_TYPE), ("commit", COMMIT_CUSTOM_TYPE)] {
        let error = storage.get_entry(id).await.unwrap_err();
        assert!(
            error.to_string().contains("native body digest mismatch"),
            "{error}"
        );
        let q = EntryQuery {
            custom_type: Some(kind.into()),
            ..Default::default()
        };
        let error = storage.find_entries(&q).await.unwrap_err();
        assert!(
            error.to_string().contains("native body digest mismatch"),
            "{error}"
        );
        assert!(storage
            .find_entries(&EntryQuery {
                exclude_tool_program_bodies: true,
                ..q
            })
            .await
            .unwrap()
            .is_empty());
    }
}
