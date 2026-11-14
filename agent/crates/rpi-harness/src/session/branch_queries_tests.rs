//! Focused branch API tests; no durable files or real session directories.
use std::sync::Arc;

use rpi_agent::message::AgentMessage;
use rpi_ai::types::{
    Api, AssistantMessage, Content, ImageContent, StopReason, Usage, UserContent, UserMessage,
};

use super::branch_queries::overview_from_metadata;
use super::memory::{CounterIdGenerator, FakeClock, InMemorySessionStorage};
use super::state::EntryMeta;
use super::types::*;
use super::Session;
use crate::error::SessionErrorCode;

fn fixture() -> (Arc<InMemorySessionStorage>, Session) {
    let storage = Arc::new(InMemorySessionStorage::new(
        SessionMetadata {
            id: "branch-api-test".into(),
            created_at: 0,
            parent_session_id: None,
        },
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    ));
    let session = Session::new(storage.clone(), None);
    (storage, session)
}
async fn append(
    storage: &InMemorySessionStorage,
    lane: &str,
    id: &str,
    kind: ProvisionedKind,
) -> Entry {
    storage
        .append_entry(
            ProvisionedEntry {
                id: id.into(),
                kind,
            },
            lane,
        )
        .await
        .unwrap()
}
fn custom(text: &str) -> ProvisionedKind {
    ProvisionedKind::Custom {
        custom_type: "test".into(),
        data: Some(serde_json::json!({"text": text})),
    }
}
fn image_message() -> AgentMessage {
    AgentMessage::User(UserMessage {
        role: Default::default(),
        timestamp: 7,
        content: UserContent::Blocks(vec![Content::Image(ImageContent {
            kind: Default::default(),
            data: "aGVsbG8=".into(),
            mime_type: "image/png".into(),
        })]),
    })
}
fn usage(input: i64, output: i64, read: i64, write: i64, cost: f64) -> Usage {
    let mut value = Usage::zero();
    value.input = input;
    value.output = output;
    value.cache_read = read;
    value.cache_write = write;
    value.cost.total = cost;
    value
}
fn assistant(model: &str, usage: Usage, reason: StopReason) -> ProvisionedKind {
    let mut message = AssistantMessage::empty(Api::Faux, "provider", model, 0);
    message.usage = usage;
    message.stop_reason = reason;
    ProvisionedKind::Message {
        message: AgentMessage::Assistant(Box::new(message)),
        terminate: None,
    }
}
fn compaction(usage: Option<Usage>) -> ProvisionedKind {
    ProvisionedKind::Compaction {
        summary: "compacted".into(),
        retained_tail: vec![image_message()],
        tokens_before: 123,
        details: Some(serde_json::json!({"kept": true})),
        usage,
    }
}
fn ids(page: &BranchEntryPage) -> Vec<&str> {
    page.entries.iter().map(Entry::id).collect()
}

#[tokio::test]
async fn complete_variants_images_and_exact_byte_budget() {
    let (storage, session) = fixture();
    let kinds = vec![
        ProvisionedKind::Message {
            message: image_message(),
            terminate: Some(false),
        },
        ProvisionedKind::ModelChange {
            provider: "p".into(),
            model_id: "m".into(),
        },
        ProvisionedKind::ThinkingLevel {
            thinking_level: "high".into(),
        },
        ProvisionedKind::ActiveTools {
            active_tool_names: vec!["read".into()],
        },
        compaction(Some(usage(1, 2, 3, 4, 0.5))),
        ProvisionedKind::BranchSummary {
            from_id: "e0".into(),
            summary: "summary".into(),
            details: Some(serde_json::json!({"note": "all fields"})),
            usage: Some(Usage::zero()),
        },
        custom("last"),
    ];
    let mut expected = Vec::new();
    for (index, kind) in kinds.into_iter().enumerate() {
        expected.push(append(&storage, "main", &format!("e{index}"), kind).await);
    }
    let full = session
        .branch_entry_page(&Default::default())
        .await
        .unwrap();
    assert_eq!(full.entries, expected);
    assert!(!full.has_more_before && !full.has_more_after);
    let total: usize = expected
        .iter()
        .map(|entry| serde_json::to_vec(entry).unwrap().len())
        .sum();
    let exact = session
        .branch_entry_page(&BranchEntryPageQuery {
            max_bytes: total,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(exact.entries, expected);
    let shorter = session
        .branch_entry_page(&BranchEntryPageQuery {
            max_bytes: total - 1,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(shorter.entries, expected[1..]);
    assert!(shorter.has_more_before);
    // Every variant, including retained image blocks, survives a tiny budget.
    for entry in &expected {
        let single = session
            .branch_entry_page(&BranchEntryPageQuery {
                leaf_id: Some(entry.id().into()),
                max_bytes: 1,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(single.entries, vec![entry.clone()]);
        assert_eq!(single.leaf_id.as_deref(), Some(entry.id()));
    }
}

#[tokio::test]
async fn oversized_entry_is_alone_and_pagination_never_drops_it() {
    let (storage, session) = fixture();
    let first = append(&storage, "main", "first", custom("small")).await;
    let large = append(
        &storage,
        "main",
        "large",
        custom(&"x\n\"".repeat(32 * 1024)),
    )
    .await;
    let last = append(&storage, "main", "last", custom("small")).await;
    let budget = serde_json::to_vec(&first)
        .unwrap()
        .len()
        .max(serde_json::to_vec(&last).unwrap().len())
        + 1;
    let latest = session
        .branch_entry_page(&BranchEntryPageQuery {
            max_bytes: budget,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(ids(&latest), ["last"]);
    assert_eq!(latest.next_before_id.as_deref(), Some("last"));
    let middle = session
        .branch_entry_page(&BranchEntryPageQuery {
            before_id: latest.next_before_id,
            leaf_id: latest.leaf_id,
            max_bytes: budget,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(middle.entries, [large.clone()]);
    assert!(middle.has_more_before && middle.has_more_after);
    assert_eq!(middle.next_after_id.as_deref(), Some("large"));
    let forward = session
        .branch_entry_page(&BranchEntryPageQuery {
            after_id: Some("first".into()),
            max_bytes: budget,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(forward.entries, [large]);
    let root = session
        .branch_entry_page(&BranchEntryPageQuery {
            before_id: Some("large".into()),
            limit: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(root.entries, [first]);
    assert!(!root.has_more_before && root.has_more_after);
    let end = session
        .branch_entry_page(&BranchEntryPageQuery {
            after_id: Some("last".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(end.entries.is_empty() && end.has_more_before && !end.has_more_after);
    assert!(end.next_before_id.is_none());
    let start = session
        .branch_entry_page(&BranchEntryPageQuery {
            before_id: Some("first".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(start.entries.is_empty() && !start.has_more_before && start.has_more_after);
}

#[tokio::test]
async fn fixed_leaf_cursor_validation_and_abandoned_branch() {
    let (storage, session) = fixture();
    append(&storage, "main", "root", custom("root")).await;
    storage.create_lane("other", Some("root")).await.unwrap();
    append(&storage, "main", "old", custom("old")).await;
    let pinned = session
        .branch_entry_page(&Default::default())
        .await
        .unwrap()
        .leaf_id;
    append(&storage, "main", "new", custom("new")).await;
    assert_eq!(
        ids(&session
            .branch_entry_page(&BranchEntryPageQuery {
                leaf_id: pinned.clone(),
                ..Default::default()
            })
            .await
            .unwrap()),
        ["root", "old"]
    );
    append(&storage, "other", "sibling", custom("sibling")).await;
    let other = session.view("other");
    assert_eq!(
        ids(&other.branch_entry_page(&Default::default()).await.unwrap()),
        ["root", "sibling"]
    );
    for id in ["sibling", "missing"] {
        let error = session
            .branch_entry_page(&BranchEntryPageQuery {
                before_id: Some(id.into()),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, SessionErrorCode::InvalidQuery);
        assert_eq!(error.message, "cursor must belong to the selected branch");
    }
    let error = session
        .branch_entry_page(&BranchEntryPageQuery {
            leaf_id: pinned.clone(),
            after_id: Some("new".into()),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.message, "cursor must belong to the selected branch");
    storage.move_lane("main", Some("root")).await.unwrap();
    append(&storage, "main", "replacement", custom("replacement")).await;
    let error = session
        .branch_entry_page(&BranchEntryPageQuery {
            leaf_id: pinned,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, SessionErrorCode::InvalidQuery);
    assert!(error.message.starts_with("branch_changed:"));
    assert_eq!(
        ids(&other.branch_entry_page(&Default::default()).await.unwrap()),
        ["root", "sibling"]
    );
}

#[tokio::test]
async fn defaults_empty_lanes_and_query_limits() {
    let (storage, session) = fixture();
    let defaults = BranchEntryPageQuery::default();
    assert_eq!(defaults.limit, 50);
    assert_eq!(defaults.max_bytes, 4 * 1024 * 1024);
    let empty = session.branch_entry_page(&defaults).await.unwrap();
    assert!(empty.entries.is_empty() && empty.leaf_id.is_none());
    let overview = session.branch_overview().await.unwrap();
    assert_eq!(overview.thinking_level, "off");
    assert!(overview.model.is_none() && overview.active_tool_names.is_none());
    assert_eq!(overview.input, 0);
    for query in [
        BranchEntryPageQuery {
            limit: 0,
            ..Default::default()
        },
        BranchEntryPageQuery {
            limit: 101,
            ..Default::default()
        },
        BranchEntryPageQuery {
            max_bytes: 0,
            ..Default::default()
        },
        BranchEntryPageQuery {
            max_bytes: 16 * 1024 * 1024 + 1,
            ..Default::default()
        },
        BranchEntryPageQuery {
            before_id: Some("x".into()),
            after_id: Some("y".into()),
            ..Default::default()
        },
        BranchEntryPageQuery {
            leaf_id: Some(String::new()),
            ..Default::default()
        },
        BranchEntryPageQuery {
            before_id: Some("bad\n".into()),
            ..Default::default()
        },
        BranchEntryPageQuery {
            after_id: Some("x".repeat(513)),
            ..Default::default()
        },
    ] {
        assert_eq!(
            session.branch_entry_page(&query).await.unwrap_err().code,
            SessionErrorCode::InvalidQuery
        );
    }
    assert_eq!(
        session
            .branch_overview_for_lane("absent")
            .await
            .unwrap_err()
            .code,
        SessionErrorCode::InvalidLane
    );
    assert_eq!(
        session
            .branch_entry_page_for_lane("absent", &defaults)
            .await
            .unwrap_err()
            .code,
        SessionErrorCode::InvalidLane
    );
    assert_eq!(
        session
            .branch_entry_page(&BranchEntryPageQuery {
                before_id: Some("absent".into()),
                ..Default::default()
            })
            .await
            .unwrap_err()
            .code,
        SessionErrorCode::InvalidQuery
    );
    for i in 0..105 {
        append(&storage, "main", &format!("e{i}"), custom("small")).await;
    }
    let page = session.branch_entry_page(&defaults).await.unwrap();
    assert_eq!(page.entries.len(), 50);
    assert_eq!(page.first_id.as_deref(), Some("e55"));
    assert_eq!(page.last_id.as_deref(), Some("e104"));
    assert!(page.has_more_before && !page.has_more_after);
    let max = session
        .branch_entry_page(&BranchEntryPageQuery {
            limit: 100,
            max_bytes: 16 * 1024 * 1024,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(max.entries.len(), 100);
}

#[tokio::test]
async fn overview_settings_usage_and_compaction_are_branch_scoped() {
    let (storage, session) = fixture();
    append(
        &storage,
        "main",
        "model",
        ProvisionedKind::ModelChange {
            provider: "initial".into(),
            model_id: "configured".into(),
        },
    )
    .await;
    append(
        &storage,
        "main",
        "thinking",
        ProvisionedKind::ThinkingLevel {
            thinking_level: "high".into(),
        },
    )
    .await;
    append(
        &storage,
        "main",
        "tools",
        ProvisionedKind::ActiveTools {
            active_tool_names: vec!["read".into()],
        },
    )
    .await;
    append(
        &storage,
        "main",
        "success",
        assistant("reported", usage(10, 2, 30, 0, 0.25), StopReason::Stop),
    )
    .await;
    storage.create_lane("other", Some("success")).await.unwrap();
    assert_eq!(
        session.branch_overview().await.unwrap().context_tokens,
        Some(42)
    );
    // Totals span the full branch, while compaction clears only context size.
    append(
        &storage,
        "main",
        "compact",
        compaction(Some(usage(900, 900, 900, 900, 900.0))),
    )
    .await;
    let compacted = session.branch_overview().await.unwrap();
    assert_eq!(compacted.context_tokens, None);
    assert_eq!(
        compacted.model,
        Some(("provider".into(), "reported".into()))
    );
    assert_eq!(compacted.thinking_level, "high");
    assert_eq!(compacted.active_tool_names, Some(vec!["read".into()]));
    assert_eq!(compacted.input, 10);
    assert_eq!(compacted.cache_hit_rate, Some(75.0));
    append(
        &storage,
        "main",
        "abort",
        assistant("aborted-model", usage(5, 1, 0, 5, 0.5), StopReason::Aborted),
    )
    .await;
    append(
        &storage,
        "main",
        "error",
        assistant("error-model", usage(2, 3, 0, 0, 0.125), StopReason::Error),
    )
    .await;
    let aborted = session.branch_overview().await.unwrap();
    assert_eq!(aborted.context_tokens, None);
    assert_eq!(
        aborted.model,
        Some(("provider".into(), "error-model".into()))
    );
    assert_eq!(aborted.cache_hit_rate, Some(0.0)); // error without cache does not replace it
    assert_eq!(
        (
            aborted.input,
            aborted.output,
            aborted.cache_read,
            aborted.cache_write
        ),
        (17, 6, 30, 5)
    );
    assert_eq!(aborted.cost_total, 0.875);
    append(
        &storage,
        "main",
        "new-model",
        ProvisionedKind::ModelChange {
            provider: "chosen".into(),
            model_id: "new".into(),
        },
    )
    .await;
    append(
        &storage,
        "main",
        "off",
        ProvisionedKind::ThinkingLevel {
            thinking_level: "off".into(),
        },
    )
    .await;
    append(
        &storage,
        "main",
        "no-tools",
        ProvisionedKind::ActiveTools {
            active_tool_names: vec![],
        },
    )
    .await;
    // Cross multiple default-read batches without building provider context.
    for i in 0..37 {
        append(&storage, "main", &format!("padding{i}"), custom("no usage")).await;
    }
    let overview = session.branch_overview().await.unwrap();
    assert_eq!(overview.model, Some(("chosen".into(), "new".into())));
    assert_eq!(overview.thinking_level, "off");
    assert_eq!(overview.active_tool_names, Some(vec![]));
    let other = session.view("other").branch_overview().await.unwrap();
    assert_eq!(other.context_tokens, Some(42));
    assert_eq!(other.input, 10);
    assert_eq!(other.thinking_level, "high");
    let entries = storage
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
            "padding36",
        )
        .await
        .unwrap();
    let metadata: Vec<_> = entries.iter().map(EntryMeta::from_entry).collect();
    assert_eq!(overview_from_metadata(&metadata), overview);
    append(
        &storage,
        "main",
        "resumed",
        assistant("resumed", usage(4, 1, 0, 0, 0.0), StopReason::Stop),
    )
    .await;
    assert_eq!(
        session.branch_overview().await.unwrap().context_tokens,
        Some(5)
    );
    append(
        &storage,
        "main",
        "zero",
        assistant("zero", Usage::zero(), StopReason::Stop),
    )
    .await;
    assert_eq!(
        session.branch_overview().await.unwrap().context_tokens,
        Some(5)
    );
}
