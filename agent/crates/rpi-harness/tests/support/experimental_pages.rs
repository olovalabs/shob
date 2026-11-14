use super::*;
use rpi_ai::types::{Api, AssistantMessage, Content, UserContent, UserMessage};
use rpi_harness::error::SessionResult;

fn compare_summary(a: SessionResult<MessageSummaryPage>, b: SessionResult<MessageSummaryPage>) {
    match (a, b) {
        (Ok(a), Ok(b)) => assert_eq!(
            serde_json::to_value(a).unwrap(),
            serde_json::to_value(b).unwrap()
        ),
        (Err(a), Err(b)) => {
            assert_eq!(a.code, b.code);
            assert_eq!(
                a.message.contains("branch_changed"),
                b.message.contains("branch_changed")
            );
        }
        pair => panic!("summary mismatch: {pair:?}"),
    }
}
fn compare_window(a: SessionResult<MessageWindowPage>, b: SessionResult<MessageWindowPage>) {
    match (a, b) {
        (Ok(a), Ok(b)) => {
            assert_eq!(a.entries, b.entries);
            assert_eq!(a.first_id, b.first_id);
            assert_eq!(a.last_id, b.last_id);
            assert_eq!(a.next_before_id, b.next_before_id);
            assert_eq!(a.next_after_id, b.next_after_id);
            assert_eq!(a.has_more_before, b.has_more_before);
            assert_eq!(a.has_more_after, b.has_more_after);
            assert_eq!(a.leaf_id, b.leaf_id);
        }
        (Err(a), Err(b)) => {
            assert_eq!(a.code, b.code);
            assert_eq!(
                a.message.contains("branch_changed"),
                b.message.contains("branch_changed")
            );
        }
        pair => panic!("window mismatch: {pair:?}"),
    }
}
fn item(n: usize) -> ProvisionedEntry {
    let kind = match n % 7 {
        0 | 4 => ProvisionedKind::Message {
            message: UserMessage::new(
                UserContent::Blocks(vec![
                    Content::text(" 猫🐈 \n".repeat(3000)),
                    serde_json::from_value(
                        json!({"type":"image","mimeType":"image/png","data":"must-not-leak"}),
                    )
                    .unwrap(),
                ]),
                1000 + n as i64,
            )
            .into(),
            terminate: None,
        },
        1 => {
            let mut assistant =
                AssistantMessage::empty(Api::AnthropicMessages, "fixture", "fixture", 7);
            assistant
                .content
                .push(Content::text("unselected assistant text"));
            ProvisionedKind::Message {
                message: rpi_agent::AgentMessage::Assistant(Box::new(assistant)),
                terminate: None,
            }
        }
        2 => ProvisionedKind::ModelChange {
            provider: "fixture".into(),
            model_id: "fixture".into(),
        },
        3 => ProvisionedKind::Compaction {
            summary: "compact".into(),
            retained_tail: vec![],
            tokens_before: 100,
            details: None,
            usage: None,
        },
        5 => ProvisionedKind::BranchSummary {
            from_id: "entry-0".into(),
            summary: "branch summary is not transcript".into(),
            details: None,
            usage: None,
        },
        _ => ProvisionedKind::Message {
            message: rpi_agent::AgentMessage::Custom(rpi_agent::message::CustomMessage {
                role: "user".into(),
                content: vec![Content::text("custom role is not a real user")],
                data: json!({}),
                timestamp: 0,
            }),
            terminate: None,
        },
    };
    ProvisionedEntry {
        id: format!("entry-{n}"),
        kind,
    }
}
#[tokio::test]
async fn summary_and_window_matrix_matches_memory_across_branches_and_reopen() {
    let (_d, repo, s) = fixture().await;
    let memory = InMemorySessionStorage::new(
        s.metadata(),
        Arc::new(FixedClock),
        Arc::new(CounterIdGenerator::new()),
    );
    for backend in [&s as &dyn SessionStorage, &memory as &dyn SessionStorage] {
        for n in 0..18 {
            backend.append_entry(item(n), "main").await.unwrap();
        }
        backend.create_lane("side", Some("entry-4")).await.unwrap();
        backend.append_entry(item(18), "side").await.unwrap();
        backend.append_entry(item(19), "side").await.unwrap();
        backend.create_lane("empty", None).await.unwrap();
    }
    let metadata = s.metadata();
    drop(s);
    let s = repo.open(&metadata).await.unwrap();
    for lane in ["main", "side", "empty", "missing"] {
        for pinned in [
            None,
            Some("entry-0"),
            Some("entry-9"),
            Some("entry-19"),
            Some("absent"),
        ] {
            for cap in [0, 1, 2, 3, 7, 100, 101] {
                for before in [
                    None,
                    Some("entry-0"),
                    Some("entry-4"),
                    Some("entry-9"),
                    Some("entry-18"),
                    Some("entry-19"),
                    Some("absent"),
                ] {
                    let query = MessageSummaryQuery {
                        leaf_id: pinned.map(str::to_owned),
                        before_id: before.map(str::to_owned),
                        limit: cap,
                        ..Default::default()
                    };
                    compare_summary(
                        s.message_summaries(lane, &query).await,
                        memory.message_summaries(lane, &query).await,
                    );
                }
                for cursor in [
                    "entry-0", "entry-4", "entry-9", "entry-17", "entry-19", "absent",
                ] {
                    for mode in 0..3 {
                        let query = MessageWindowQuery {
                            leaf_id: pinned.map(str::to_owned),
                            limit: cap,
                            anchor_id: (mode == 0).then(|| cursor.into()),
                            before_id: (mode == 1).then(|| cursor.into()),
                            after_id: (mode == 2).then(|| cursor.into()),
                        };
                        compare_window(
                            s.message_window(lane, &query).await,
                            memory.message_window(lane, &query).await,
                        );
                    }
                }
            }
        }
    }
    for backend in [&s as &dyn SessionStorage, &memory as &dyn SessionStorage] {
        backend.move_lane("main", Some("entry-4")).await.unwrap();
    }
    let query = MessageSummaryQuery {
        leaf_id: Some("entry-17".into()),
        ..Default::default()
    };
    compare_summary(
        s.message_summaries("main", &query).await,
        memory.message_summaries("main", &query).await,
    );
    let page = s
        .message_summaries("side", &Default::default())
        .await
        .unwrap();
    assert!(!serde_json::to_string(&page)
        .unwrap()
        .contains("must-not-leak"));
    assert!(page
        .messages
        .iter()
        .all(|m| m.timestamp >= 1000 && m.has_images));
}

#[tokio::test]
async fn malformed_page_inputs_and_error_precedence_match_memory() {
    let (_d, _repo, s) = fixture().await;
    let memory = InMemorySessionStorage::new(
        s.metadata(),
        Arc::new(FixedClock),
        Arc::new(CounterIdGenerator::new()),
    );
    let long = "x".repeat(513);
    for lane in ["main", "missing"] {
        for pinned in [None, Some(""), Some("\n"), Some("missing")] {
            for malformed in ["", "\n", long.as_str()] {
                for role in ["user", "assistant", ""] {
                    let query = MessageSummaryQuery {
                        role: role.into(),
                        before_id: Some(malformed.into()),
                        leaf_id: pinned.map(str::to_owned),
                        ..Default::default()
                    };
                    compare_summary(
                        s.message_summaries(lane, &query).await,
                        memory.message_summaries(lane, &query).await,
                    );
                }
                for bits in 0..8 {
                    let query = MessageWindowQuery {
                        anchor_id: (bits & 1 != 0).then(|| malformed.into()),
                        before_id: (bits & 2 != 0).then(|| malformed.into()),
                        after_id: (bits & 4 != 0).then(|| malformed.into()),
                        leaf_id: pinned.map(str::to_owned),
                        ..Default::default()
                    };
                    compare_window(
                        s.message_window(lane, &query).await,
                        memory.message_window(lane, &query).await,
                    );
                }
            }
        }
    }
}
