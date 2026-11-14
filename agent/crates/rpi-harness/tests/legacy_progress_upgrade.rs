//! Explicit locked native-JSONL upgrade, never implicit artifact migration.
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_harness::agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome};
use rpi_harness::session::jsonl::{
    encode_header, encode_mutation, JsonlSessionStorage, JsonlV4Header,
};
use rpi_harness::session::types::*;
use rpi_harness::session::{DefaultIdGenerator, Session, SystemClock};
use rpi_harness::types::AgentHarnessOptions;
use rpi_tools::{FileContent, FileSystem, InMemoryExecutionEnv};
use std::sync::Arc;

const PATH: &str = "/legacy.jsonl";
const SIDE: &str = "/legacy.jsonl.progress.jsonl";
fn frame(id: &str) -> AssistantFrameRecord {
    AssistantFrameRecord {
        base: RecordBase {
            id: id.into(),
            lane: "main".into(),
            seq: 0,
            timestamp: 0,
        },
        run_id: "old-run".into(),
        stream_index: 0,
        op: AssistantFrameOp::Append,
        frame: Some(
            serde_json::json!({"type":"text_delta", "contentIndex":0,"delta":"old payload"}),
        ),
    }
}
async fn fixture() -> Arc<dyn FileSystem> {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryExecutionEnv::with_cwd("/".into()));
    let header = JsonlV4Header::new("legacy-id".into(), 1, "/".into(), None, None, None);
    fs.write_file(PATH, FileContent::Text(encode_header(&header)), None)
        .await
        .unwrap();
    let storage = load(fs.clone()).await;
    storage
        .append_entry(
            ProvisionedEntry {
                id: "old-user".into(),
                kind: ProvisionedKind::Message {
                    message: rpi_ai::UserMessage::new("old question", 1).into(),
                    terminate: None,
                },
            },
            "main",
        )
        .await
        .unwrap();
    // A genuine 0.16.0 frame remains in history, not copied into the companion.
    storage
        .append_record(LaneRecord::AssistantFrame(frame("historical-frame")))
        .await
        .unwrap();
    storage.drain().await;
    fs
}
async fn load(fs: Arc<dyn FileSystem>) -> JsonlSessionStorage {
    JsonlSessionStorage::load(
        fs,
        PATH,
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    )
    .await
    .unwrap()
}
fn checkpoint(id: &str, seq: u64) -> Vec<u8> {
    format!(
        "{}\n",
        serde_json::json!({"format":"assistant-progress-v1", "session_id":id,
        "state":{"sequence":seq,"frames":[]}})
    )
    .into_bytes()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_progress_upgrade_preserves_history_then_runs_across_restarts() {
    let fs = fixture().await;
    let original = fs.read_text_file(PATH, None).await.unwrap();
    let plain = load(fs.clone()).await;
    let entries = plain.find_entries(&EntryQuery::default()).await.unwrap();
    let records = plain.find_records(&RecordQuery::default()).await.unwrap();
    assert!(plain.append_progress(vec![frame("refused")]).await.is_err());
    assert!(!fs.exists(SIDE, None).await.unwrap());
    // A previous history-only read must not leave an empty cached progress view.
    assert!(plain.read_progress("old-run").await.unwrap().is_empty());
    let storage = plain.enable_progress_under_exclusive_lock().await.unwrap();
    assert_eq!(storage.metadata().id, "legacy-id");
    assert_eq!(
        storage.find_entries(&EntryQuery::default()).await.unwrap(),
        entries
    );
    assert_eq!(
        storage.find_records(&RecordQuery::default()).await.unwrap(),
        records
    );
    let upgraded = fs.read_text_file(PATH, None).await.unwrap();
    assert_eq!(
        original.split_once('\n').unwrap().1,
        upgraded.split_once('\n').unwrap().1
    );
    assert!(storage.read_progress("old-run").await.unwrap().is_empty());
    assert!(!fs
        .read_text_file(SIDE, None)
        .await
        .unwrap()
        .contains("old payload"));
    drop(storage);
    for _ in 0..2 {
        let storage = load(fs.clone())
            .await
            .enable_progress_under_exclusive_lock()
            .await
            .unwrap();
        let provider = FauxProvider::new(FauxScript::new().with_text("continued"));
        let harness = AgentHarness::create(AgentHarnessOptions {
            model: provider.default_model().clone(),
            models: vec![provider],
            session: Session::new(Arc::new(storage), None),
            allow_existing_session: true,
            ..Default::default()
        })
        .await
        .unwrap();
        let run = harness.prompt_text("continue", vec![]).await.unwrap();
        assert!(matches!(run.outcome, HarnessRunOutcome::Completed { .. }));
        assert_eq!(harness.session().storage().metadata().id, "legacy-id");
        assert!(harness
            .session()
            .find_open_operations("main", None)
            .await
            .unwrap()
            .is_empty());
    }
    let final_text = fs.read_text_file(PATH, None).await.unwrap();
    assert!(final_text
        .split_once('\n')
        .unwrap()
        .1
        .starts_with(original.split_once('\n').unwrap().1));
    let mut previous = 0;
    for line in final_text.lines().skip(1) {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        let seq = value["seq"].as_u64().unwrap();
        assert!(seq > previous);
        previous = seq;
    }
}

#[tokio::test]
async fn matching_empty_checkpoint_is_reused_but_all_other_companions_are_untouched() {
    let fs = fixture().await;
    let empty = checkpoint("legacy-id", 0);
    fs.write_file(SIDE, FileContent::Bytes(empty.clone()), None)
        .await
        .unwrap();
    load(fs.clone())
        .await
        .enable_progress_under_exclusive_lock()
        .await
        .unwrap();
    assert_eq!(fs.read_binary_file(SIDE, None).await.unwrap(), empty);
    let pretty = format!(
        "{}\n",
        serde_json::to_string_pretty(&serde_json::from_slice::<serde_json::Value>(&empty).unwrap())
            .unwrap()
    )
    .into_bytes();
    for bytes in [
        checkpoint("foreign", 0),
        checkpoint("legacy-id", 4),
        b"broken\n".to_vec(),
        [checkpoint("legacy-id", 0), b"{\"payload\":true}\n".to_vec()].concat(),
        [checkpoint("legacy-id", 0), b"\n".to_vec()].concat(),
        pretty,
        Vec::new(),
    ] {
        let fs = fixture().await;
        let before = fs.read_binary_file(PATH, None).await.unwrap();
        fs.write_file(SIDE, FileContent::Bytes(bytes.clone()), None)
            .await
            .unwrap();
        assert!(load(fs.clone())
            .await
            .enable_progress_under_exclusive_lock()
            .await
            .is_err());
        assert_eq!(fs.read_binary_file(SIDE, None).await.unwrap(), bytes);
        assert_eq!(fs.read_binary_file(PATH, None).await.unwrap(), before);
    }
}

#[tokio::test]
async fn changed_history_and_shared_handles_are_refused_and_declared_missing_stays_closed() {
    let fs = fixture().await;
    let storage = load(fs.clone()).await;
    let clone = storage.clone();
    assert!(storage
        .enable_progress_under_exclusive_lock()
        .await
        .is_err());
    drop(clone);
    assert!(!fs.exists(SIDE, None).await.unwrap());
    let storage = load(fs.clone()).await;
    let extra = encode_mutation(&SessionMutation::Lane {
        seq: 50,
        lane: "other".into(),
        leaf_id: None,
    });
    fs.append_file(PATH, FileContent::Text(extra), None)
        .await
        .unwrap();
    let changed = fs.read_binary_file(PATH, None).await.unwrap();
    assert!(storage
        .enable_progress_under_exclusive_lock()
        .await
        .is_err());
    assert_eq!(fs.read_binary_file(PATH, None).await.unwrap(), changed);
    assert!(!fs.exists(SIDE, None).await.unwrap());
    load(fs.clone())
        .await
        .enable_progress_under_exclusive_lock()
        .await
        .unwrap();
    fs.remove(SIDE, false, false, None).await.unwrap();
    assert!(JsonlSessionStorage::load(
        fs,
        PATH,
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new())
    )
    .await
    .is_err());
}

use async_trait::async_trait;
use rpi_tools::env::FileInfo;
use rpi_tools::error::{FileError, FileErrorCode};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;
struct UpgradeFs {
    inner: Arc<dyn FileSystem>,
    mode: AtomicUsize,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    published: tokio::sync::Notify,
}
impl UpgradeFs {
    fn new(inner: Arc<dyn FileSystem>, mode: usize) -> Arc<Self> {
        Arc::new(Self {
            inner,
            mode: AtomicUsize::new(mode),
            entered: Default::default(),
            release: Default::default(),
            published: Default::default(),
        })
    }
}
#[async_trait]
impl FileSystem for UpgradeFs {
    fn cwd(&self) -> &std::path::Path {
        self.inner.cwd()
    }

    async fn absolute_path(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<std::path::PathBuf, FileError> {
        self.inner.absolute_path(path, cancel).await
    }

    async fn join_path(
        &self,
        parts: &[&str],
        cancel: Option<&CancellationToken>,
    ) -> Result<std::path::PathBuf, FileError> {
        self.inner.join_path(parts, cancel).await
    }

    async fn read_text_file(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<String, FileError> {
        self.inner.read_text_file(path, cancel).await
    }

    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<String>, FileError> {
        self.inner.read_text_lines(path, max_lines, cancel).await
    }

    async fn read_binary_file(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<u8>, FileError> {
        self.inner.read_binary_file(path, cancel).await
    }

    async fn write_file(
        &self,
        path: &str,
        content: FileContent,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        if path == format!("{SIDE}.tmp") && self.mode.load(Ordering::SeqCst) == 6 {
            self.mode.store(0, Ordering::SeqCst);
            return Err(FileError::new(
                FileErrorCode::Unknown,
                "sidecar staging failure",
            ));
        }
        if path == format!("{PATH}.tmp") && self.mode.load(Ordering::SeqCst) == 1 {
            self.mode.store(0, Ordering::SeqCst);
            return Err(FileError::new(
                FileErrorCode::Unknown,
                "header staging failure",
            ));
        }
        if path == format!("{PATH}.tmp") && self.mode.load(Ordering::SeqCst) == 5 {
            self.mode.store(0, Ordering::SeqCst);
            self.inner
                .append_file(
                    PATH,
                    FileContent::Text("changed during upgrade\n".into()),
                    None,
                )
                .await?;
        }
        self.inner.write_file(path, content, cancel).await
    }

    async fn append_file(
        &self,
        path: &str,
        content: FileContent,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        self.inner.append_file(path, content, cancel).await
    }

    async fn rename_file(
        &self,
        source: &str,
        dest: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        if dest == SIDE && self.mode.load(Ordering::SeqCst) == 7 {
            self.mode.store(0, Ordering::SeqCst);
            return Err(FileError::new(
                FileErrorCode::Unknown,
                "sidecar rename failure",
            ));
        }
        if dest != PATH {
            return self.inner.rename_file(source, dest, cancel).await;
        }
        let mode = self.mode.swap(0, Ordering::SeqCst);
        if mode == 2 {
            return Err(FileError::new(
                FileErrorCode::Unknown,
                "header rename failure",
            ));
        }
        if mode == 4 {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.inner.rename_file(source, dest, cancel).await?;
        self.published.notify_one();
        if mode == 3 {
            return Err(FileError::new(
                FileErrorCode::Unknown,
                "rename landed but reported failure",
            ));
        }
        Ok(())
    }

    async fn file_info(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<FileInfo, FileError> {
        self.inner.file_info(path, cancel).await
    }

    async fn list_dir(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<FileInfo>, FileError> {
        self.inner.list_dir(path, cancel).await
    }

    async fn canonical_path(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<std::path::PathBuf, FileError> {
        self.inner.canonical_path(path, cancel).await
    }

    async fn exists(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<bool, FileError> {
        self.inner.exists(path, cancel).await
    }

    async fn create_dir(
        &self,
        path: &str,
        recursive: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        self.inner.create_dir(path, recursive, cancel).await
    }

    async fn remove(
        &self,
        path: &str,
        recursive: bool,
        force: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        self.inner.remove(path, recursive, force, cancel).await
    }

    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        cancel: Option<&CancellationToken>,
    ) -> Result<std::path::PathBuf, FileError> {
        self.inner.create_temp_dir(prefix, cancel).await
    }

    async fn create_temp_file(
        &self,
        prefix: &str,
        suffix: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<std::path::PathBuf, FileError> {
        self.inner.create_temp_file(prefix, suffix, cancel).await
    }

    async fn cleanup(&self) {
        self.inner.cleanup().await
    }
}

#[tokio::test]
async fn failed_header_stage_or_rename_retries_without_reinitializing_checkpoint() {
    for mode in [1, 2, 3] {
        let base = fixture().await;
        let original = base.read_binary_file(PATH, None).await.unwrap();
        let fs = UpgradeFs::new(base.clone(), mode);
        assert!(load(fs.clone())
            .await
            .enable_progress_under_exclusive_lock()
            .await
            .is_err());
        let side = base.read_binary_file(SIDE, None).await.unwrap();
        if mode != 3 {
            assert_eq!(base.read_binary_file(PATH, None).await.unwrap(), original);
        }
        assert!(!base.exists(&format!("{PATH}.tmp"), None).await.unwrap());
        let storage = load(fs.clone())
            .await
            .enable_progress_under_exclusive_lock()
            .await
            .unwrap();
        assert_eq!(base.read_binary_file(SIDE, None).await.unwrap(), side);
        storage
            .append_progress(vec![frame("new-progress")])
            .await
            .unwrap();
        assert_eq!(
            load(fs).await.read_progress("old-run").await.unwrap().len(),
            1
        );
    }
}
#[tokio::test]
async fn cancellation_during_header_rename_finishes_owned_upgrade() {
    let fs = UpgradeFs::new(fixture().await, 4);
    let storage = load(fs.clone()).await;
    let task = tokio::spawn(storage.enable_progress_under_exclusive_lock());
    fs.entered.notified().await;
    task.abort();
    match task.await {
        Err(error) => assert!(error.is_cancelled()),
        Ok(_) => panic!("caller should be cancelled"),
    }
    fs.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), fs.published.notified())
        .await
        .unwrap();
    let storage = load(fs.clone()).await;
    storage
        .append_progress(vec![frame("after-cancellation")])
        .await
        .unwrap();
    assert_eq!(
        load(fs).await.read_progress("old-run").await.unwrap().len(),
        1
    );
}
#[tokio::test]
async fn history_changed_during_upgrade_is_never_overwritten() {
    let fs = UpgradeFs::new(fixture().await, 5);
    assert!(load(fs.clone())
        .await
        .enable_progress_under_exclusive_lock()
        .await
        .is_err());
    let text = fs.read_text_file(PATH, None).await.unwrap();
    assert!(text.ends_with(
        "changed during upgrade
"
    ));
    assert!(!text.lines().next().unwrap().contains("progressFormat"));
    let actual: serde_json::Value =
        serde_json::from_slice(&fs.read_binary_file(SIDE, None).await.unwrap()).unwrap();
    assert_eq!(
        actual,
        serde_json::from_slice::<serde_json::Value>(&checkpoint("legacy-id", 0)).unwrap()
    );
}

#[tokio::test]
async fn sidecar_publication_failures_leave_history_unmarked_and_retryable() {
    for mode in [6, 7] {
        let base = fixture().await;
        let history = base.read_binary_file(PATH, None).await.unwrap();
        let fs = UpgradeFs::new(base.clone(), mode);
        assert!(load(fs.clone())
            .await
            .enable_progress_under_exclusive_lock()
            .await
            .is_err());
        assert_eq!(base.read_binary_file(PATH, None).await.unwrap(), history);
        assert!(!base.exists(SIDE, None).await.unwrap());
        assert!(!base.exists(&format!("{SIDE}.tmp"), None).await.unwrap());
        load(fs)
            .await
            .enable_progress_under_exclusive_lock()
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn undeclared_valid_live_checkpoint_is_not_adopted_or_cleared() {
    let fs = fixture().await;
    let history = fs.read_binary_file(PATH, None).await.unwrap();
    let mut payload = frame("live-frame");
    payload.base.seq = 1;
    let bytes = format!(
        "{}\n",
        serde_json::json!({
            "format":"assistant-progress-v1", "session_id":"legacy-id",
            "state":{"sequence":1,"frames":[payload]}
        })
    )
    .into_bytes();
    fs.write_file(SIDE, FileContent::Bytes(bytes.clone()), None)
        .await
        .unwrap();
    assert!(load(fs.clone())
        .await
        .enable_progress_under_exclusive_lock()
        .await
        .is_err());
    assert_eq!(fs.read_binary_file(SIDE, None).await.unwrap(), bytes);
    assert_eq!(fs.read_binary_file(PATH, None).await.unwrap(), history);
}
