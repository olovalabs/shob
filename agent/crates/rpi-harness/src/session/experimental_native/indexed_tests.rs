use super::*;
use crate::session::experimental_native::ExperimentalNativeRepo;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::sync::atomic::{AtomicUsize, Ordering};

async fn fixture() -> (tempfile::TempDir, ExperimentalNativeStorage) {
    let dir = tempfile::tempdir().unwrap();
    let repo = ExperimentalNativeRepo::new(dir.path().to_owned())
        .await
        .unwrap();
    let storage = repo.create(&SessionCreateOptions::default()).await.unwrap();
    (dir, storage)
}
fn user(n: usize) -> ProvisionedEntry {
    ProvisionedEntry {
        id: format!("u{n}"),
        kind: ProvisionedKind::Message {
            message: rpi_ai::UserMessage::new(format!("user {n}"), 42).into(),
            terminate: None,
        },
    }
}
async fn users(storage: &ExperimentalNativeStorage, count: usize) {
    storage
        .run(move |s, clock| {
            commit(
                s,
                (0..count)
                    .map(|n| Write::Entry(user(n), "main".into()))
                    .collect(),
                None,
                clock,
            )?;
            Ok(())
        })
        .await
        .unwrap();
}
#[tokio::test]
async fn summaries_never_authorize_canonical_body_reads_and_windows_only_read_selected_bodies() {
    let (_dir, s) = fixture().await;
    users(&s, 32).await;
    s.run(|s, _| {
        s.forbid_bodies(true);
        Ok(())
    })
    .await
    .unwrap();
    let summary = s
        .message_summaries(
            "main",
            &MessageSummaryQuery {
                limit: 2,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(summary.first_id.as_deref(), Some("u30"));
    assert!(s
        .message_window(
            "main",
            &MessageWindowQuery {
                anchor_id: Some("u31".into()),
                limit: 1,
                ..Default::default()
            }
        )
        .await
        .is_err());
    s.run(|s, _| {
        s.forbid_bodies(false);
        s.connection.execute(
            "UPDATE facts SET body='damaged lookahead/unselected body' WHERE seq<32",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    let window = s
        .message_window(
            "main",
            &MessageWindowQuery {
                anchor_id: Some("u31".into()),
                limit: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(window.entries[0].id(), "u31");
    assert!(window.has_more_before);
    assert!(s
        .message_window(
            "main",
            &MessageWindowQuery {
                anchor_id: Some("u31".into()),
                limit: 2,
                ..Default::default()
            }
        )
        .await
        .is_err());
    assert_eq!(
        s.message_summaries("main", &Default::default())
            .await
            .unwrap()
            .messages
            .len(),
        10
    );
}
#[tokio::test]
async fn selected_sql_queries_do_not_decode_unrelated_retired_metadata() {
    let (_dir, s) = fixture().await;
    users(&s, 64).await;
    s.run(|s,clock|{
        let records=(0..32).map(|n|Write::Record(serde_json::from_value(serde_json::json!({"id":format!("abort{n}"),"lane":"main","seq":0,"timestamp":0,"type":"abort_requested","runId":"closed-run"})).unwrap())).collect();
        commit(s,records,None,clock)?;
        s.connection.execute("UPDATE facts SET metadata='damaged retired metadata' WHERE seq=1",[])?;Ok(())
    }).await.unwrap();
    assert_eq!(
        s.find_entries(&EntryQuery {
            limit: Some(2),
            ..Default::default()
        })
        .await
        .unwrap()
        .len(),
        2
    );
    assert_eq!(
        s.find_entries(&EntryQuery {
            custom_type: Some("absent".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .len(),
        0
    );
    assert_eq!(
        s.find_records(&RecordQuery {
            record_type: Some("abort_requested"),
            limit: Some(2),
            ..Default::default()
        })
        .await
        .unwrap()
        .len(),
        2
    );
    let page = s
        .record_page(&RecordPageQuery {
            lane: "main".into(),
            limit: 2,
            from_end: true,
            after_seq: None,
            before_seq: None,
        })
        .await
        .unwrap();
    assert_eq!(page.records.len(), 2);
    assert_eq!(page.total, 32);
    assert_eq!(
        s.get_log(&LogOptions {
            after_seq: Some(94),
            limit: Some(2)
        })
        .await
        .unwrap()
        .len(),
        2
    );
    assert!(s.get_entry("u0").await.is_err());
    s.run(|s, _| {
        s.connection
            .execute("UPDATE selection SET keys='{}' WHERE seq=64", [])?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(s
        .find_entries(&EntryQuery {
            limit: Some(1),
            ..Default::default()
        })
        .await
        .is_err());
}
#[tokio::test]
async fn fixed_latest_page_metadata_point_reads_do_not_grow_with_retired_history() {
    let mut counts = Vec::new();
    for n in [32, 512] {
        let (_dir, s) = fixture().await;
        users(&s, n).await;
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        s.run(move |s, _| {
            s.connection.authorizer(Some(move |ctx: AuthContext<'_>| {
                if matches!(
                    ctx.action,
                    AuthAction::Read {
                        table_name: "facts",
                        column_name: "metadata"
                    }
                ) {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                Authorization::Allow
            }));
            Ok(())
        })
        .await
        .unwrap();
        s.find_entries(&EntryQuery {
            limit: Some(2),
            ..Default::default()
        })
        .await
        .unwrap();
        s.message_summaries(
            "main",
            &MessageSummaryQuery {
                limit: 2,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        s.message_window(
            "main",
            &MessageWindowQuery {
                anchor_id: Some(format!("u{}", n - 1)),
                limit: 2,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        counts.push(count.load(Ordering::Relaxed));
    }
    assert!(counts[0] > 0);
    assert_eq!(
        counts[0], counts[1],
        "counts measure prepared metadata point reads, not physical I/O"
    );
}
#[tokio::test]
async fn ui_index_publication_failure_rolls_back_fact_navigation_root_and_sequence() {
    let (_dir, s) = fixture().await;
    s.run(|s, _| {
        s.connection.authorizer(Some(|ctx: AuthContext<'_>| {
            if matches!(
                ctx.action,
                AuthAction::Insert {
                    table_name: "ui_summary"
                }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }));
        Ok(())
    })
    .await
    .unwrap();
    assert!(s.append_entry(user(0), "main").await.is_err());
    s.run(|s, _| {
        s.connection
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        for table in [
            "facts",
            "selection",
            "tool_keys",
            "navigation",
            "ui_summary",
        ] {
            let count: u64 =
                s.connection
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
            assert_eq!(count, 0, "{table}");
        }
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(s.history_revision().await.unwrap(), 0);
    assert_eq!(s.append_entry(user(0), "main").await.unwrap().seq(), 1);
}
#[tokio::test]
async fn damaged_summary_and_navigation_are_rejected_without_body_fallback() {
    for table in ["ui_summary", "navigation"] {
        let (_dir, s) = fixture().await;
        users(&s, 32).await;
        let table = table.to_owned();
        s.run(move |s, _| {
            s.connection
                .execute(&format!("UPDATE {table} SET json='{{}}' WHERE seq=32"), [])?;
            s.forbid_bodies(true);
            Ok(())
        })
        .await
        .unwrap();
        assert!(s
            .message_summaries(
                "main",
                &MessageSummaryQuery {
                    limit: 1,
                    ..Default::default()
                }
            )
            .await
            .is_err());
    }
}

#[path = "index_guard_tests.rs"]
mod guards;
