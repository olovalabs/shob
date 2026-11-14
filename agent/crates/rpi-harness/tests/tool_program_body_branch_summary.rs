//! Branch summaries retain exact archival ancestry without decoding its bodies.
//! Poison after cold indexing: these are pinned-read probes, not startup/RSS claims.
use std::{path::Path, sync::Arc};

use rpi_harness::{
    compaction::branch_summary::{collect_entries_for_branch_summary, prepare_branch_entries},
    session::{
        jsonl::{JsonlSessionStorage, JsonlV4Header},
        types::*,
        DefaultIdGenerator, InMemorySessionStorage, Session, SqliteSessionRepo,
    },
    tool_program_body::{CHUNK_CUSTOM_TYPE, COMMIT_CUSTOM_TYPE},
};
use serde_json::json;

struct FixedClock;
impl rpi_harness::session::memory::Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        42
    }
}
fn memory() -> Session {
    Session::new(
        Arc::new(InMemorySessionStorage::new(
            SessionMetadata {
                id: "branch-body".into(),
                created_at: 0,
                parent_session_id: None,
            },
            Arc::new(FixedClock),
            Arc::new(DefaultIdGenerator::new()),
        )),
        None,
    )
}
async fn append(s: &Session, lane: &str, id: &str, kind: ProvisionedKind) {
    s.append_entry(
        ProvisionedEntry {
            id: id.into(),
            kind,
        },
        lane,
    )
    .await
    .unwrap();
}
async fn user(s: &Session, lane: &str, id: &str) {
    append(
        s,
        lane,
        id,
        ProvisionedKind::Message {
            message: rpi_ai::UserMessage::new(id, 42).into(),
            terminate: None,
        },
    )
    .await;
}
async fn custom(s: &Session, lane: &str, id: &str, kind: &str) {
    append(
        s,
        lane,
        id,
        ProvisionedKind::Custom {
            custom_type: kind.into(),
            data: Some(json!({"payload": "x".repeat(48 * 1024)})),
        },
    )
    .await;
}
async fn fixture(s: &Session, lca: &str) {
    user(s, "main", "root").await;
    for id in ["common-chunk-0", "common-chunk-1", "common-chunk-2"] {
        custom(s, "main", id, CHUNK_CUSTOM_TYPE).await;
    }
    custom(s, "main", "common-commit", COMMIT_CUSTOM_TYPE).await;
    s.move_lane("main", Some(lca)).await.unwrap();
    user(s, "main", "old-1").await;
    custom(s, "main", "ordinary", "extension").await;
    custom(s, "main", "prefix", "rpi.tool_program.body_chunk.v1.extra").await;
    for id in ["old-chunk-0", "old-chunk-1"] {
        custom(s, "main", id, CHUNK_CUSTOM_TYPE).await;
    }
    custom(s, "main", "old-commit", COMMIT_CUSTOM_TYPE).await;
    append(
        s,
        "main",
        "thinking",
        ProvisionedKind::ThinkingLevel {
            thinking_level: "high".into(),
        },
    )
    .await;
    user(s, "main", "old-2").await;
    s.create_lane("side", Some(lca)).await.unwrap();
    user(s, "side", "target").await;
    custom(s, "side", "target-chunk", CHUNK_CUSTOM_TYPE).await;
    custom(s, "side", "target-commit", COMMIT_CUSTOM_TYPE).await;
    s.create_lane("unrelated", None).await.unwrap();
    user(s, "unrelated", "other-root").await;
}
fn ids(entries: &[Entry]) -> Vec<&str> {
    entries.iter().map(Entry::id).collect()
}
fn bounds(start: &str) -> BranchBounds {
    BranchBounds {
        start: Some(start.into()),
        ..Default::default()
    }
}
async fn assert_summary(s: &Session, lca: &str) {
    // Explicit target lies on a non-main lane. Its archival tip must not decode.
    let result = collect_entries_for_branch_summary(s, Some("old-2"), "target-commit")
        .await
        .unwrap();
    assert_eq!(result.common_ancestor_id.as_deref(), Some(lca));
    assert_eq!(
        ids(&result.entries),
        ["old-1", "ordinary", "prefix", "thinking", "old-2"]
    );
    assert!(
        matches!(&result.entries[1], Entry::Custom(e) if e.custom_type == "extension" && e.data.as_ref().unwrap()["payload"].as_str().unwrap().len() == 48 * 1024)
    );
    assert!(matches!(&result.entries[3], Entry::ThinkingLevel(e) if e.thinking_level == "high"));
    assert_eq!(prepare_branch_entries(&result.entries, 0).messages.len(), 2);

    let path = s.branch_ancestry(&bounds("old-2")).await.unwrap();
    assert_eq!(
        path[0],
        EntryAncestry {
            id: "old-2".into(),
            parent_id: Some("thinking".into())
        }
    );
    assert_eq!(
        path.last().unwrap(),
        &EntryAncestry {
            id: "root".into(),
            parent_id: None
        }
    );
    let ancestor = path.iter().position(|entry| entry.id == lca).unwrap();
    assert_eq!(path[ancestor - 1].id, "old-1");
    assert_eq!(path[ancestor - 1].parent_id.as_deref(), Some(lca));
    let mut expected = vec![
        "old-2",
        "thinking",
        "old-commit",
        "old-chunk-1",
        "old-chunk-0",
        "prefix",
        "ordinary",
        "old-1",
    ];
    if lca == "common-commit" {
        expected.extend(["common-commit", "common-chunk-2"]);
    }
    expected.extend(["common-chunk-1", "common-chunk-0", "root"]);
    assert_eq!(
        path.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        expected
    );
    for pair in path.windows(2) {
        assert_eq!(pair[0].parent_id.as_deref(), Some(pair[1].id.as_str()));
    }
    let bounded = s
        .branch_ancestry(&BranchBounds {
            stop_at_id: Some(lca.into()),
            ..bounds("old-2")
        })
        .await
        .unwrap();
    assert_eq!(bounded, path[..=ancestor]);
    let by_type = s
        .branch_ancestry(&BranchBounds {
            stop_at_type: Some("custom"),
            ..bounds("old-2")
        })
        .await
        .unwrap();
    assert_eq!(
        by_type.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
        ["old-2", "thinking", "old-commit"]
    );
    let side = s
        .view("side")
        .branch_ancestry(&BranchBounds::default())
        .await
        .unwrap();
    assert_eq!(side[0].id, "target-commit");
    assert_eq!(side[1].id, "target-chunk");
    assert_eq!(side[2].parent_id.as_deref(), Some(lca));

    // Same leaf, ancestor target, archival leaf, and root are all inclusive in
    // ancestry but exclusive in the returned abandoned semantic tail.
    for (old, target, common, expected) in [
        ("old-2", "old-2", "old-2", vec![]),
        (
            "old-2",
            "old-1",
            "old-1",
            vec!["ordinary", "prefix", "thinking", "old-2"],
        ),
        (lca, "target", lca, vec![]),
        ("root", "target", "root", vec![]),
    ] {
        let r = collect_entries_for_branch_summary(s, Some(old), target)
            .await
            .unwrap();
        assert_eq!(r.common_ancestor_id.as_deref(), Some(common));
        assert_eq!(ids(&r.entries), expected);
    }
    let none = collect_entries_for_branch_summary(s, None, "does-not-exist")
        .await
        .unwrap();
    assert!(none.entries.is_empty());
    assert_eq!(none.common_ancestor_id, None);
    let disjoint = collect_entries_for_branch_summary(s, Some("old-2"), "other-root")
        .await
        .unwrap();
    assert_eq!(disjoint.common_ancestor_id, None);
    assert_eq!(
        ids(&disjoint.entries),
        ["root", "old-1", "ordinary", "prefix", "thinking", "old-2"]
    );
    assert!(s.branch_ancestry(&bounds("does-not-exist")).await.is_err());
    assert!(
        collect_entries_for_branch_summary(s, Some("old-2"), "does-not-exist")
            .await
            .is_err()
    );
    s.create_lane("empty", None).await.unwrap();
    assert!(s
        .view("empty")
        .branch_ancestry(&BranchBounds::default())
        .await
        .unwrap()
        .is_empty());
}
async fn jsonl(path: &str, dir: &Path, load: bool) -> Session {
    let env = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(dir.into()));
    let store = if load {
        JsonlSessionStorage::load(
            env,
            path,
            Arc::new(FixedClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap()
    } else {
        JsonlSessionStorage::create(
            env,
            path,
            JsonlV4Header::new("branch-body".into(), 42, "/".into(), None, None, None),
            Arc::new(FixedClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .await
        .unwrap()
    };
    Session::new(Arc::new(store), None)
}
#[tokio::test]
async fn cold_jsonl_poisoned_archival_chunk_and_commit_are_exact_common_ancestors() {
    for lca in ["common-chunk-1", "common-commit"] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp
            .path()
            .join("branch.jsonl")
            .to_string_lossy()
            .into_owned();
        let s = jsonl(&path, temp.path(), false).await;
        fixture(&s, lca).await;
        s.drain().await;
        drop(s);
        let s = jsonl(&path, temp.path(), true).await;
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut count = 0;
        let poisoned = raw
            .lines()
            .map(|line| {
                if line.contains(CHUNK_CUSTOM_TYPE) || line.contains(COMMIT_CUSTOM_TYPE) {
                    // The ordinary prefix-match custom entry is not an archive.
                    let row: serde_json::Value = serde_json::from_str(line).unwrap();
                    let kind = row["customType"].as_str();
                    if kind == Some(CHUNK_CUSTOM_TYPE) || kind == Some(COMMIT_CUSTOM_TYPE) {
                        count += 1;
                        assert!(line.contains("xxxxxxxx"));
                        return line.replacen("xxxxxxxx", "POISONED", 1);
                    }
                }
                line.to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        assert_eq!(count, 9);
        assert_eq!(raw.len(), poisoned.len());
        std::fs::write(&path, poisoned).unwrap();
        assert_summary(&s, lca).await;
        for id in [
            "common-chunk-0",
            "common-chunk-1",
            "common-chunk-2",
            "common-commit",
            "old-chunk-0",
            "old-chunk-1",
            "old-commit",
            "target-chunk",
            "target-commit",
        ] {
            assert!(
                s.get_entry(id).await.is_err(),
                "explicit read must fail: {id}"
            );
        }
        assert!(s.find_entries(&EntryQuery::default()).await.is_err());
        for kind in [CHUNK_CUSTOM_TYPE, COMMIT_CUSTOM_TYPE] {
            assert!(s
                .find_entries(&EntryQuery {
                    custom_type: Some(kind.into()),
                    ..Default::default()
                })
                .await
                .is_err());
        }
    }
}
#[tokio::test]
async fn memory_and_sqlite_keep_ancestry_and_ordinary_metadata_semantics() {
    for lca in ["common-chunk-1", "common-commit"] {
        let temp = tempfile::tempdir().unwrap();
        let sqlite = SqliteSessionRepo::with_clock_ids(
            temp.path().join("branch.db"),
            Arc::new(FixedClock),
            Arc::new(DefaultIdGenerator::new()),
        )
        .unwrap()
        .create(&SessionCreateOptions {
            id: Some("branch-body".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        for s in [memory(), Session::new(Arc::new(sqlite), None)] {
            fixture(&s, lca).await;
            assert_summary(&s, lca).await;
            assert_eq!(s.get_entry(lca).await.unwrap().unwrap().id(), lca);
            assert_eq!(
                s.find_entries(&EntryQuery {
                    custom_type: Some(COMMIT_CUSTOM_TYPE.into()),
                    ..Default::default()
                })
                .await
                .unwrap()
                .len(),
                3
            );
        }
    }
}
#[cfg(feature = "experimental-native-store")]
#[tokio::test]
async fn experimental_native_poisoned_bodies_preserve_metadata_ancestry() {
    use rpi_harness::session::experimental_native::ExperimentalNativeRepo;
    for lca in ["common-chunk-1", "common-commit"] {
        let temp = tempfile::tempdir().unwrap();
        let repo = ExperimentalNativeRepo::with_clock(temp.path().into(), Arc::new(FixedClock))
            .await
            .unwrap();
        let storage = repo
            .create(&SessionCreateOptions {
                id: Some("branch-body".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let s = Session::new(Arc::new(storage), None);
        fixture(&s, lca).await;
        s.drain().await;
        // Body digests remain pinned, metadata and its digest remain intact.
        let conn = rusqlite::Connection::open(temp.path().join("branch-body.rpinative")).unwrap();
        let changed = conn.execute("UPDATE facts SET body=replace(body,'xxxxxxxx','POISONED') WHERE id IN ('common-chunk-0','common-chunk-1','common-chunk-2','common-commit','old-chunk-0','old-chunk-1','old-commit','target-chunk','target-commit')", []).unwrap();
        assert_eq!(changed, 9);
        drop(conn);
        assert_summary(&s, lca).await;
        assert!(s.get_entry(lca).await.is_err());
        assert!(s.find_entries(&EntryQuery::default()).await.is_err());
    }
}
