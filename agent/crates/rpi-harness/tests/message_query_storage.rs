//! Built-in storage parity and reopen checks; no harness run/provider/tool calls.
use rpi_ai::types::UserMessage;
use rpi_harness::session::jsonl::{
    JsonlSessionCreateOptions, JsonlSessionRepo, JsonlSessionRepoOptions,
};
use rpi_harness::session::session::DefaultIdGenerator;
use rpi_harness::session::types::{
    MessageSummaryQuery, MessageWindowQuery, SessionCreateOptions, SessionRepo, SessionStorage,
};
use rpi_harness::session::{
    CounterIdGenerator, InMemorySessionRepo, Session, SqliteSessionRepo, SystemClock,
};
use rpi_tools::{FileSystem, OsExecutionEnv};
use std::sync::Arc;
async fn fill(storage: Arc<dyn SessionStorage>) {
    let session = Session::new(storage.clone(), None);
    for index in 0..23 {
        session
            .append_message(UserMessage::new(format!("Question {index}"), index).into())
            .await
            .unwrap();
    }
    storage.drain().await;
}
async fn check(storage: &dyn SessionStorage) {
    let first = storage
        .message_summaries("main", &Default::default())
        .await
        .unwrap();
    assert_eq!(first.messages.len(), 10);
    assert_eq!(first.messages.first().unwrap().preview, "Question 13");
    let second = storage
        .message_summaries(
            "main",
            &MessageSummaryQuery {
                before_id: first.next_before_id.clone(),
                leaf_id: first.leaf_id.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(second.messages.first().unwrap().preview, "Question 3");
    let third = storage
        .message_summaries(
            "main",
            &MessageSummaryQuery {
                before_id: second.next_before_id,
                leaf_id: first.leaf_id.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(third.messages.len(), 3);
    assert!(!third.has_more);
    let window = storage
        .message_window(
            "main",
            &MessageWindowQuery {
                anchor_id: second.first_id.clone(),
                leaf_id: first.leaf_id,
                limit: 4,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(window.entries.len(), 4);
    assert!(window
        .entries
        .iter()
        .any(|entry| Some(entry.id()) == second.first_id.as_deref()));
}
#[tokio::test]
async fn memory_user_summary_pages() {
    let repo = InMemorySessionRepo::new(Arc::new(SystemClock), Arc::new(CounterIdGenerator::new()));
    let storage = repo.create(&SessionCreateOptions::default()).await.unwrap();
    fill(Arc::new(storage.clone())).await;
    check(&storage).await;
}
#[tokio::test]
async fn sqlite_reopen_rebuilds_user_index_and_keeps_context_cursors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("query.sqlite");
    let repo = SqliteSessionRepo::open(&path).unwrap();
    let storage = repo.create(&SessionCreateOptions::default()).await.unwrap();
    let metadata = storage.metadata();
    fill(Arc::new(storage)).await;
    drop(repo);
    let repo = SqliteSessionRepo::open(&path).unwrap();
    let storage = repo.open(&metadata).await.unwrap();
    check(&storage).await;
}
#[tokio::test]
async fn jsonl_replay_rebuilds_user_index_and_keeps_context_cursors() {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(dir.path().to_path_buf()));
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs,
        sessions_root: dir.path().join("sessions").to_string_lossy().into_owned(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });
    let storage = repo
        .create_typed(&JsonlSessionCreateOptions {
            id: None,
            parent_session_id: None,
            cwd: dir.path().to_string_lossy().into_owned(),
            metadata: None,
        })
        .await
        .unwrap();
    let metadata = storage.metadata();
    fill(Arc::new(storage)).await;
    let storage = repo.open(&metadata).await.unwrap();
    check(&storage).await;
}
