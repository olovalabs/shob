//! Shared repository contract: exercise the library surface, not CLI policy.
//! Every durable fixture lives below an independently owned temporary directory.

use std::sync::Arc;

use rpi_agent::message::AgentMessage;
use serde_json::json;

use super::jsonl::{JsonlSessionRepo, JsonlSessionRepoOptions};
use super::memory::{CounterIdGenerator, FakeClock, InMemorySessionRepo};
use super::sqlite::SqliteSessionRepo;
use super::types::*;

fn options(id: &str) -> SessionCreateOptions {
    SessionCreateOptions {
        id: Some(id.into()),
        ..Default::default()
    }
}

fn user_message(text: &str) -> AgentMessage {
    AgentMessage::from(rpi_ai::types::UserMessage::new(text, 42))
}

fn message(id: &str, text: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.into(),
        kind: ProvisionedKind::Message {
            message: user_message(text),
            terminate: None,
        },
    }
}

fn base(id: &str) -> RecordBase {
    RecordBase {
        id: id.into(),
        seq: 0,
        timestamp: 0,
        lane: "main".into(),
    }
}

fn frame(id: &str, run: &str) -> AssistantFrameRecord {
    AssistantFrameRecord {
        base: base(id),
        run_id: run.into(),
        stream_index: 0,
        op: AssistantFrameOp::Append,
        frame: Some(json!({"delta": id})),
    }
}

async fn branch(storage: &dyn SessionStorage, leaf: &str) -> Vec<Entry> {
    storage
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &BranchBounds::default(),
            leaf,
        )
        .await
        .unwrap()
}

fn ids(entries: &[Entry]) -> Vec<&str> {
    entries.iter().map(Entry::id).collect()
}

async fn history_contract(storage: &dyn SessionStorage) {
    let mut accepted = storage
        .append_entry(message("root", "shared root"), "main")
        .await
        .unwrap();
    let original = accepted.clone();
    // Both the write result and read result must be independent owned values.
    if let Entry::Message(entry) = &mut accepted {
        entry.message = AgentMessage::from(rpi_ai::types::UserMessage::new("changed", 99));
        entry.base.id = "changed".into();
    } else {
        panic!("message append returned a non-message entry");
    }
    let mut read = storage.get_entry("root").await.unwrap().unwrap();
    assert_eq!(read, original);
    if let Entry::Message(entry) = &mut read {
        entry.message = AgentMessage::from(rpi_ai::types::UserMessage::new("changed again", 99));
    }
    assert_eq!(storage.get_entry("root").await.unwrap(), Some(original));

    storage.create_lane("side", Some("root")).await.unwrap();
    storage
        .append_entry(message("main-child", "main only"), "main")
        .await
        .unwrap();
    storage
        .append_entry(message("side-child", "side only"), "side")
        .await
        .unwrap();
    assert_eq!(
        ids(&branch(storage, "main-child").await),
        ["root", "main-child"]
    );
    let side = branch(storage, "side-child").await;
    assert_eq!(ids(&side), ["root", "side-child"]);
    let context = storage
        .context_on_branch(&BranchBounds::default(), "side-child")
        .await
        .unwrap();
    assert_eq!(context.messages.len(), 2);
    assert_eq!(context.messages[0], user_message("shared root"));
    assert_eq!(context.messages[1], user_message("side only"));

    storage.move_lane("side", Some("root")).await.unwrap();
    assert_eq!(
        storage
            .get_lanes()
            .await
            .unwrap()
            .iter()
            .find(|p| p.lane == "side")
            .unwrap()
            .leaf_id
            .as_deref(),
        Some("root")
    );
    // Moving a lane is navigation, not deletion of the abandoned branch.
    assert_eq!(
        ids(&branch(storage, "side-child").await),
        ["root", "side-child"]
    );
    storage.move_lane("side", Some("side-child")).await.unwrap();

    storage.set_name(Some("first")).await.unwrap();
    storage.set_name(Some("contract session")).await.unwrap();
    storage
        .set_label("root", Some("first label"))
        .await
        .unwrap();
    storage.set_label("root", Some("anchor")).await.unwrap();
    assert_eq!(
        storage.get_name().await.unwrap().as_deref(),
        Some("contract session")
    );
    assert_eq!(
        storage.get_label("root").await.unwrap().as_deref(),
        Some("anchor")
    );
    storage.set_label("root", None).await.unwrap();
    assert_eq!(storage.get_label("root").await.unwrap(), None);
    storage.set_label("root", Some("anchor")).await.unwrap();
    storage.set_name(None).await.unwrap();
    assert_eq!(storage.get_name().await.unwrap(), None);
    storage.set_name(Some("contract session")).await.unwrap();
}

async fn progress_contract(storage: &dyn SessionStorage) {
    storage
        .append_record(LaneRecord::OperationStarted(OperationStartedRecord {
            base: base("run"),
            source_leaf_id: Some("main-child".into()),
            intent: OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
                resume_data: None,
            },
        }))
        .await
        .unwrap();
    let before = storage.get_log(&LogOptions::default()).await.unwrap();
    let stamped = storage
        .append_progress(vec![
            frame("frame-done", "run"),
            frame("frame-live", "live-run"),
        ])
        .await
        .unwrap();
    assert_eq!(
        stamped.iter().map(|f| f.base.seq).collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(
        storage.get_log(&LogOptions::default()).await.unwrap(),
        before
    );
    assert_eq!(storage.prune_completed_progress().await.unwrap(), 0);
    let mut read = storage.read_progress("run").await.unwrap();
    assert_eq!(read, vec![stamped[0].clone()]);
    read[0].frame = Some(json!({"delta": "changed copy"}));
    assert_eq!(
        storage.read_progress("run").await.unwrap(),
        vec![stamped[0].clone()]
    );

    let finish = storage
        .append_record(LaneRecord::OperationFinished(OperationFinishedRecord {
            base: base("finish"),
            run_id: "run".into(),
            outcome: OperationOutcome::Completed,
            error: None,
            result: None,
        }))
        .await
        .unwrap();
    assert_eq!(finish.seq(), before.last().unwrap().seq() + 1);
    let before_prune = storage.get_log(&LogOptions::default()).await.unwrap();
    assert_eq!(storage.prune_completed_progress().await.unwrap(), 1);
    assert_eq!(storage.prune_completed_progress().await.unwrap(), 0);
    assert!(storage.read_progress("run").await.unwrap().is_empty());
    assert_eq!(
        storage.read_progress("live-run").await.unwrap(),
        vec![stamped[1].clone()]
    );
    assert_eq!(
        storage.get_log(&LogOptions::default()).await.unwrap(),
        before_prune
    );
    let next = storage
        .append_progress(vec![frame("frame-next", "live-run")])
        .await
        .unwrap();
    assert_eq!(
        next[0].base.seq, 3,
        "retirement must not reset progress sequence"
    );
    let entry = storage
        .append_entry(message("after-progress", "history resumes"), "main")
        .await
        .unwrap();
    assert_eq!(entry.seq(), finish.seq() + 1);
}

async fn repo_contract<R: SessionRepo>(repo: R)
where
    R::Storage: 'static,
{
    let storage: Arc<dyn SessionStorage> = Arc::new(repo.create(&options("source")).await.unwrap());
    let metadata = storage.metadata();
    assert_eq!(metadata.id, "source");
    assert_eq!(metadata.parent_session_id, None);
    history_contract(storage.as_ref()).await;
    progress_contract(storage.as_ref()).await;
    storage.drain().await;
    drop(storage);

    let reopened: Arc<dyn SessionStorage> = Arc::new(repo.open(&metadata).await.unwrap());
    assert_eq!(reopened.metadata(), metadata);
    assert_eq!(
        ids(&branch(reopened.as_ref(), "main-child").await),
        ["root", "main-child"]
    );
    assert_eq!(
        reopened.get_name().await.unwrap().as_deref(),
        Some("contract session")
    );
    assert_eq!(
        reopened.get_label("root").await.unwrap().as_deref(),
        Some("anchor")
    );
    assert!(reopened.read_progress("run").await.unwrap().is_empty());
    assert_eq!(reopened.read_progress("live-run").await.unwrap().len(), 2);
    drop(reopened);

    // Explicit library fork options are valid even when a CLI mode is fresh-only.
    let fork = repo
        .fork(
            &metadata,
            &options("branch-fork"),
            &ForkOptions::Branch {
                entry_id: Some("main-child".into()),
                position: Some(ForkPosition::At),
            },
        )
        .await
        .unwrap();
    assert_eq!(fork.metadata().parent_session_id.as_deref(), Some("source"));
    assert_eq!(
        ids(&branch(&fork, "main-child").await),
        ["root", "main-child"]
    );
    assert!(fork.get_entry("side-child").await.unwrap().is_none());
    assert!(fork.read_progress("live-run").await.unwrap().is_empty());
    fork.set_name(Some("fork only")).await.unwrap();
    fork.drain().await;
    drop(fork);

    let tree = repo
        .fork(&metadata, &options("tree-fork"), &ForkOptions::Tree)
        .await
        .unwrap();
    assert_eq!(
        ids(&branch(&tree, "side-child").await),
        ["root", "side-child"]
    );
    assert!(tree
        .get_lanes()
        .await
        .unwrap()
        .iter()
        .any(|p| p.lane == "side" && p.leaf_id.as_deref() == Some("side-child")));
    tree.drain().await;
    drop(tree);
    let source = repo.open(&metadata).await.unwrap();
    assert_eq!(
        source.get_name().await.unwrap().as_deref(),
        Some("contract session")
    );
    source.drain().await;
}

#[tokio::test]
async fn memory_repo_contract() {
    repo_contract(InMemorySessionRepo::new(
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    ))
    .await;
}

#[tokio::test]
async fn os_jsonl_repo_contract() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_string_lossy().into_owned();
    let repo = JsonlSessionRepo::new(
        JsonlSessionRepoOptions {
            fs: Arc::new(rpi_tools::os_env::OsExecutionEnv::with_cwd(
                dir.path().to_owned(),
            )),
            sessions_root: dir.path().join("sessions").to_string_lossy().into_owned(),
            clock: Arc::new(FakeClock::new()),
            ids: Arc::new(CounterIdGenerator::new()),
        },
        cwd,
    );
    repo_contract(repo).await;
    dir.close().unwrap();
}

#[tokio::test]
async fn sqlite_repo_contract() {
    let dir = tempfile::tempdir().unwrap();
    let repo = SqliteSessionRepo::with_clock_ids(
        dir.path().join("sessions.db"),
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    )
    .unwrap();
    repo_contract(repo).await;
    dir.close().unwrap();
}

#[cfg(feature = "experimental-native-store")]
#[tokio::test]
async fn experimental_native_repo_contract() {
    let dir = tempfile::tempdir().unwrap();
    let repo = super::experimental_native::ExperimentalNativeRepo::with_clock(
        dir.path().to_owned(),
        Arc::new(FakeClock::new()),
    )
    .await
    .unwrap();
    repo_contract(repo).await;
    dir.close().unwrap();
}
