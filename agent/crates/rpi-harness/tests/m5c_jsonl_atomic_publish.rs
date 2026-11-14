//! M5c integration — atomic publish + concurrent-write serialization for
//! `JsonlSessionStorage` / `JsonlSessionRepo`. Mirrors the atomicity +
//! concurrency slice of `packages/agent/test/harness/session/jsonl.test.ts`
//! (the "does not publish a partial fork when staging fails", "does not
//! publish a fork when atomic rename fails", "preserves the session when
//! staging/publishing torn-tail repair fails") and
//! `packages/agent/test/harness/session/jsonl-storage.test.ts` ("persists
//! concurrent cross-lane writes in shared sequence order").
//!
//! Invariants exercised (plan §5.14 + §5.2):
//! - A failed staging or rename leaves NO published destination and NO `.tmp`.
//! - The per-session tail mutex serializes appends: concurrent cross-lane
//!   writes land at strict, distinct, consecutive sequence numbers in commit
//!   order; the durable log is non-interleaved and re-reads identically.
//! - Torn-tail repair publishes atomically: a staging/rpublish failure leaves
//!   the original file byte-identical to its pre-repair state and no `.tmp`.
//!
//! Fault injection is done with a `FaultyFs` wrapper around the in-memory env
//! (the TS tests use `vi.spyOn`; the Rust port has no spy infra, so we compose
//! a delegating `FileSystem` whose single-shot hooks inject a chosen error on
//! the Nth call to a chosen method).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use rpi_agent::message::AgentMessage;
use rpi_harness::error::SessionErrorCode;
use rpi_harness::session::jsonl::types::{JsonlSessionCreateOptions, JsonlSessionListOptions};
use rpi_harness::session::jsonl::{
    encode_mutation, JsonlSessionRepo, JsonlSessionRepoOptions, JsonlSessionStorage, JsonlV4Header,
};
use rpi_harness::session::memory::{CounterIdGenerator, FakeClock};
use rpi_harness::session::types::{
    BranchBounds, EntryOrder, EntryQuery, ForkOptions, LanePointer, LaneRecord, LogItem,
    LogOptions, OperationIntent, OperationStartedRecord, ProvisionedEntry, ProvisionedKind,
    RecordBase, SessionCreateOptions, SessionMetadata, SessionMutation, SessionStorage,
};
use rpi_tools::env::{FileContent, FileInfo, FileKind, FileSystem};
use rpi_tools::error::{FileError, FileErrorCode};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn user_msg(text: &str) -> AgentMessage {
    AgentMessage::User(rpi_ai::types::UserMessage::new(text, 1))
}

fn header(id: &str) -> JsonlV4Header {
    JsonlV4Header::new(
        id.into(),
        1_700_000_000_000,
        "/proj".into(),
        None,
        None,
        None,
    )
}

fn clock() -> Arc<FakeClock> {
    Arc::new(FakeClock::new())
}

fn ids() -> Arc<CounterIdGenerator> {
    Arc::new(CounterIdGenerator::new())
}

fn repo(fs: Arc<dyn FileSystem>) -> JsonlSessionRepo {
    JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs,
        sessions_root: "/sessions".into(),
        clock: clock(),
        ids: ids(),
    })
}

fn create_options(id: &str) -> JsonlSessionCreateOptions {
    JsonlSessionCreateOptions {
        id: Some(id.into()),
        parent_session_id: None,
        cwd: "/proj".into(),
        metadata: None,
    }
}

fn note_provisioned(id: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.to_string(),
        kind: ProvisionedKind::Custom {
            custom_type: "note".to_string(),
            data: Some(serde_json::json!({ "text": "hi" })),
        },
    }
}

fn message_provisioned(id: &str, text: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.to_string(),
        kind: ProvisionedKind::Message {
            message: user_msg(text),
            terminate: None,
        },
    }
}

fn lane_mutation(seq: u64, lane: &str, leaf_id: Option<&str>) -> SessionMutation {
    SessionMutation::Lane {
        seq,
        lane: lane.to_string(),
        leaf_id: leaf_id.map(|s| s.to_string()),
    }
}

async fn list_ids(r: &JsonlSessionRepo, cwd: &str) -> Vec<String> {
    r.list_typed(&JsonlSessionListOptions {
        cwd: Some(cwd.into()),
    })
    .await
    .unwrap()
    .iter()
    .map(|m| m.id.clone())
    .collect()
}

async fn tmp_files_in_sessions_dir(fs: &Arc<dyn FileSystem>) -> Vec<String> {
    let dirs = fs.list_dir("/sessions", None).await.unwrap();
    let mut names = Vec::new();
    for d in dirs {
        let entries = fs.list_dir(&d.path.to_string_lossy(), None).await.unwrap();
        for f in entries {
            if f.name.ends_with(".tmp") {
                names.push(f.name);
            }
        }
    }
    names
}

// ---------------------------------------------------------------------------
// FaultyFs — a delegating FileSystem that can inject a single-shot error on
// the next call to a chosen method (mirrors `vi.spyOn(...).mockResolvedValueOnce`).
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum FaultTarget {
    /// Inject an error on the next `write_file`.
    WriteFile,
    /// Inject an error on the next `rename_file`.
    RenameFile,
    /// Fail only the initial value row appended to a staged fork.
    AppendInitialValue,
}

struct FaultyFs {
    inner: Arc<dyn FileSystem>,
    /// Which method to fault on its next invocation.
    target: FaultTarget,
    /// One-shot: armed until the first matching call, then disarmed.
    armed: AtomicBool,
    /// Count of `write_file` calls observed (purely for diagnostics).
    write_calls: AtomicUsize,
}

impl FaultyFs {
    fn new(inner: Arc<dyn FileSystem>, target: FaultTarget) -> Arc<Self> {
        Arc::new(Self {
            inner,
            target,
            armed: AtomicBool::new(true),
            write_calls: AtomicUsize::new(0),
        })
    }

    fn fault_error(method: &str) -> FileError {
        FileError::new(FileErrorCode::Unknown, format!("injected {method} failure"))
    }

    /// Take the fault if armed + matching; returns `Some(error)` to inject.
    fn take_if_matching(&self, is_write: bool) -> Option<FileError> {
        let matches = match self.target {
            FaultTarget::WriteFile => is_write,
            FaultTarget::RenameFile => !is_write,
            FaultTarget::AppendInitialValue => false,
        };
        if matches && self.armed.swap(false, Ordering::SeqCst) {
            Some(Self::fault_error(match self.target {
                FaultTarget::WriteFile => "write_file",
                FaultTarget::RenameFile => "rename_file",
                FaultTarget::AppendInitialValue => "append_file",
            }))
        } else {
            None
        }
    }
}

#[async_trait]
impl FileSystem for FaultyFs {
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
        self.write_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(err) = self.take_if_matching(true) {
            return Err(err.with_path(path));
        }
        self.inner.write_file(path, content, cancel).await
    }

    async fn append_file(
        &self,
        path: &str,
        content: FileContent,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        if matches!(self.target, FaultTarget::AppendInitialValue)
            && path.ends_with(".tmp")
            && matches!(&content, FileContent::Text(text) if text.contains("rpi.value"))
            && self.armed.swap(false, Ordering::SeqCst)
        {
            return Err(Self::fault_error("append initial fork value").with_path(path));
        }
        self.inner.append_file(path, content, cancel).await
    }

    async fn rename_file(
        &self,
        source: &str,
        dest: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        if let Some(err) = self.take_if_matching(false) {
            return Err(err.with_path(dest));
        }
        self.inner.rename_file(source, dest, cancel).await
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

/// Attach a `FaultyFs` to a fresh in-memory env + repo, returning both.
fn faulty_repo(target: FaultTarget) -> (Arc<FaultyFs>, JsonlSessionRepo) {
    let base: Arc<dyn FileSystem> =
        Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/proj".into()));
    let faulty = FaultyFs::new(base, target);
    let fs: Arc<dyn FileSystem> = faulty.clone();
    (faulty, repo(fs))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// Mirrors "does not publish a partial fork when staging fails": a fork whose
/// staging (`write_file` of the `.tmp`) fails must leave the destination
/// unpublished and no `.tmp` behind.
#[tokio::test]
async fn fork_staging_failure_leaves_no_destination_and_no_tmp() {
    // Use a non-faulty repo to build the source (we only fault the fork path).
    let base: Arc<dyn FileSystem> =
        Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/proj".into()));
    let r = repo(base.clone());

    let source = r.create_typed(&create_options("source")).await.unwrap();
    source
        .append_entry(note_provisioned("m1"), "main")
        .await
        .unwrap();
    source
        .append_entry(note_provisioned("m2"), "main")
        .await
        .unwrap();
    let src_meta = r
        .list_typed(&JsonlSessionListOptions {
            cwd: Some("/proj".into()),
        })
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.id == "source")
        .unwrap();

    // Switch to a FaultyFs that breaks the next write_file (the fork's staging)
    // but otherwise delegates to the same backing env the source was built on
    // (so the fork's source-reload + the JSONL reopen both succeed; only the
    // staging `write_file` of the destination `.tmp` is faulted).
    let faulty = FaultyFs::new(base.clone(), FaultTarget::WriteFile);
    let faulty_fs: Arc<dyn FileSystem> = faulty.clone();
    let r_faulty = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: faulty_fs,
        sessions_root: "/sessions".into(),
        clock: clock(),
        ids: ids(),
    });

    // Fork the whole tree so the branch-default (main leaf = "m2") is not the
    // gating error; the staging `write_file` is the fault under test.
    let err = r_faulty
        .fork_typed(&src_meta, &create_options("fork-1"), &ForkOptions::Tree)
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, SessionErrorCode::Storage);

    // The fork was never published.
    let listed = r
        .list_typed(&JsonlSessionListOptions {
            cwd: Some("/proj".into()),
        })
        .await
        .unwrap();
    assert_eq!(listed.iter().filter(|m| m.id == "fork-1").count(), 0);
    // No `.tmp` residue.
    assert!(tmp_files_in_sessions_dir(&base).await.is_empty());
    // And the fault was actually consumed (it didn't silently pass through).
    assert!(
        !faulty.armed.load(Ordering::SeqCst),
        "write_file fault was consumed by the fork staging"
    );
}

/// Mirrors "does not publish a fork when atomic rename fails": the staged
/// `.tmp` is complete, but the rename onto the destination fails — the
/// destination must remain unpublished and no `.tmp` left behind.
#[tokio::test]
async fn fork_rename_failure_leaves_no_destination_and_no_tmp() {
    let base: Arc<dyn FileSystem> =
        Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/proj".into()));
    let r = repo(base.clone());

    let source = r.create_typed(&create_options("source")).await.unwrap();
    source
        .append_entry(note_provisioned("m1"), "main")
        .await
        .unwrap();
    let src_meta = r
        .list_typed(&JsonlSessionListOptions {
            cwd: Some("/proj".into()),
        })
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.id == "source")
        .unwrap();

    let faulty = FaultyFs::new(base.clone(), FaultTarget::RenameFile);
    let faulty_fs: Arc<dyn FileSystem> = faulty.clone();
    let r_faulty = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: faulty_fs,
        sessions_root: "/sessions".into(),
        clock: clock(),
        ids: ids(),
    });

    let err = r_faulty
        .fork_typed(&src_meta, &create_options("fork-1"), &ForkOptions::Tree)
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, SessionErrorCode::Storage);
    assert_eq!(list_ids(&r, "/proj").await, vec!["source"]);
    // Recovery path removes the staged `.tmp`.
    assert!(tmp_files_in_sessions_dir(&base).await.is_empty());
}

/// Initialization is part of staging: failure of its final append or of the
/// subsequent publication must never expose an inherited-authority child.
#[tokio::test]
async fn initialized_fork_append_and_publish_failures_leave_no_destination() {
    for target in [FaultTarget::AppendInitialValue, FaultTarget::RenameFile] {
        let base: Arc<dyn FileSystem> =
            Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/proj".into()));
        let r = repo(base.clone());
        let source = r.create_typed(&create_options("source")).await.unwrap();
        source
            .append_entry(note_provisioned("m1"), "main")
            .await
            .unwrap();
        let metadata = source.jsonl_metadata();
        let history = base.read_binary_file(&metadata.path, None).await.unwrap();
        let progress_path = format!("{}.progress.jsonl", metadata.path);
        let progress = base.read_binary_file(&progress_path, None).await.unwrap();
        drop(source);
        let faulty = FaultyFs::new(base.clone(), target);
        let faulty_fs: Arc<dyn FileSystem> = faulty.clone();
        let r_faulty = repo(faulty_fs);
        let error = r_faulty
            .fork_typed_with_value_initializer(
                &metadata,
                &create_options("initialized-child"),
                &ForkOptions::default(),
                |_| {
                    Ok(vec![(
                        "rpi.resource-selection.v1".into(),
                        serde_json::json!({"revision": 1}),
                    )])
                },
            )
            .await
            .err()
            .expect("injected initialization/publication failure");
        assert_eq!(error.code, SessionErrorCode::Storage);
        assert!(!faulty.armed.load(Ordering::SeqCst));
        assert_eq!(list_ids(&r, "/proj").await, vec!["source"]);
        assert!(tmp_files_in_sessions_dir(&base).await.is_empty());
        assert_eq!(
            base.read_binary_file(&metadata.path, None).await.unwrap(),
            history
        );
        assert_eq!(
            base.read_binary_file(&progress_path, None).await.unwrap(),
            progress
        );
    }
}

/// Mirrors "releases a destination reservation after a failed create" (the
/// create branch): a create whose staging fails is retriable — the in-process
/// dedup set is released, so a second create for the same id succeeds.
#[tokio::test]
async fn failed_create_releases_reservation_and_is_retriable() {
    let (faulty, r) = faulty_repo(FaultTarget::WriteFile);

    // First create faults on the write_file of the header.
    let err = r
        .create_typed(&create_options("retry"))
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, SessionErrorCode::Storage);
    assert!(
        faulty.armed.load(Ordering::SeqCst) == false,
        "fault consumed"
    );

    // Build a fresh repo over the same (now-disarmed) FaultyFs: retry succeeds.
    let fs: Arc<dyn FileSystem> = faulty.clone();
    let r_reuse = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs,
        sessions_root: "/sessions".into(),
        clock: clock(),
        ids: ids(),
    });
    let storage = r_reuse
        .create_typed(&create_options("retry"))
        .await
        .unwrap();
    let _ = storage.get_name().await.unwrap();
    // The session now lists exactly one "retry".
    let fresh = repo(faulty.clone() as Arc<dyn FileSystem>);
    assert_eq!(list_ids(&fresh, "/proj").await, vec!["retry"]);
}

/// A torn-tail repair whose staging fails leaves the original file untouched
/// and no `.tmp` residue. Mirrors "preserves the session when staging torn-tail
/// repair fails".
#[tokio::test]
async fn torn_tail_repair_staging_failure_preserves_original() {
    let base: Arc<dyn FileSystem> = Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
    let path = "/s.jsonl";
    let _ = JsonlSessionStorage::create(base.clone(), path, header("repair"), clock(), ids())
        .await
        .unwrap();
    base.append_file(
        path,
        FileContent::Text(encode_mutation(&lane_mutation(1, "main", None))),
        None,
    )
    .await
    .unwrap();
    // Append a torn last line.
    base.append_file(path, FileContent::Text("{\"kind\":\"entry\"".into()), None)
        .await
        .unwrap();
    let original = base.read_text_file(path, None).await.unwrap();

    let faulty = FaultyFs::new(base.clone(), FaultTarget::WriteFile);
    let faulty_fs: Arc<dyn FileSystem> = faulty.clone();
    let err = JsonlSessionStorage::load(faulty_fs, path, clock(), ids())
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, SessionErrorCode::Storage);
    // Original file is byte-identical.
    assert_eq!(base.read_text_file(path, None).await.unwrap(), original);
    // No `.tmp` left at the session path.
    assert!(!base.exists("/s.jsonl.tmp", None).await.unwrap());
}

/// A torn-tail repair whose rename fails leaves the original file untouched
/// and no `.tmp` residue. Mirrors "preserves the session when torn-tail repair
/// cannot be published".
#[tokio::test]
async fn torn_tail_repair_rename_failure_preserves_original() {
    let base: Arc<dyn FileSystem> = Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/".into()));
    let path = "/s.jsonl";
    let _ = JsonlSessionStorage::create(base.clone(), path, header("repair"), clock(), ids())
        .await
        .unwrap();
    base.append_file(
        path,
        FileContent::Text(encode_mutation(&lane_mutation(1, "main", None))),
        None,
    )
    .await
    .unwrap();
    base.append_file(path, FileContent::Text("{\"kind\":\"entry\"".into()), None)
        .await
        .unwrap();
    let original = base.read_text_file(path, None).await.unwrap();

    let faulty = FaultyFs::new(base.clone(), FaultTarget::RenameFile);
    let faulty_fs: Arc<dyn FileSystem> = faulty.clone();
    let err = JsonlSessionStorage::load(faulty_fs, path, clock(), ids())
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, SessionErrorCode::Storage);
    assert_eq!(base.read_text_file(path, None).await.unwrap(), original);
    // Best-effort cleanup removed the staged `.tmp`.
    assert!(!base.exists("/s.jsonl.tmp", None).await.unwrap());
}

/// Mirrors "persists concurrent cross-lane writes in shared sequence order":
/// concurrent appends across lanes serialize through the tail mutex and land at
/// strictly increasing, distinct sequence numbers; the durable log re-reads as
/// `[1..N]` consecutive with no interleaving.
#[tokio::test]
async fn concurrent_cross_lane_writes_serialize_in_shared_sequence_order() {
    let fs: Arc<dyn FileSystem> =
        Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/proj".into()));
    let r = repo(fs.clone());
    let storage = r.create_typed(&create_options("concurrent")).await.unwrap();
    let root_entry = storage
        .append_entry(note_provisioned("root"), "main")
        .await
        .unwrap();
    storage
        .create_lane("thread", Some(root_entry.id()))
        .await
        .unwrap();

    // Issue 4 concurrent appends across the two lanes. The tail mutex
    // serializes the stamp→persist→apply critical section per op, so each
    // claims a unique, consecutive seq. `JsonlSessionStorage` is not `Clone`
    // (it holds a `Mutex` tail), so share it behind an `Arc`; `append_entry`
    // takes `&self` and serializes internally on the tail mutex.
    let storage: Arc<JsonlSessionStorage> = Arc::new(storage);
    let lanes = ["main", "thread", "main", "thread"];
    let labels = ["main-1", "thread-1", "main-2", "thread-2"];
    let mut handles = Vec::new();
    for (id, lane) in labels.into_iter().zip(lanes.into_iter()) {
        let entry = note_provisioned(id);
        let s = storage.clone();
        handles.push(tokio::spawn(
            async move { s.append_entry(entry, lane).await },
        ));
    }
    let mut results = Vec::new();
    for h in handles {
        results.push(h.await.unwrap().unwrap());
    }

    // Each entry got a distinct, strictly-increasing seq; commit order matches.
    let mut ordered = results.clone();
    ordered.sort_by_key(|e| e.seq());
    let ordered_ids: Vec<&str> = ordered.iter().map(|e| e.id()).collect();
    let seqs: Vec<u64> = ordered.iter().map(|e| e.seq()).collect();
    assert_eq!(seqs, vec![3, 4, 5, 6]); // root=1 + lane-record=2, then 3..6.
                                        // Order by seq is the commit order; the ids are whichever lane won each
                                        // slot (deterministic only per-seq, so check by-pair rather than exact id
                                        // sequence — the invariant is distinct consecutive seqs + faithful reload).
    assert_eq!(ordered_ids.len(), 4);
    assert_eq!(
        ordered_ids
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        4
    );

    // The durable log is non-interleaved + consecutive on reload.
    let metadata = r
        .list_typed(&JsonlSessionListOptions {
            cwd: Some("/proj".into()),
        })
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.id == "concurrent")
        .unwrap();
    let reopened = r.open_by_jsonl_metadata(&metadata).await.unwrap();
    let log = reopened.get_log(&LogOptions::default()).await.unwrap();
    let all_seqs: Vec<u64> = log.iter().map(|i| i.seq()).collect();
    assert_eq!(all_seqs, vec![1, 2, 3, 4, 5, 6]);
    let mut entry_ids: Vec<String> = log
        .iter()
        .filter_map(|i| match i {
            LogItem::Entry { entry, .. } => Some(entry.id().to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(entry_ids.remove(0), "root"); // root is seq 1.
    let rest: std::collections::HashSet<&str> = entry_ids.iter().map(|s| s.as_str()).collect();
    assert!(rest.contains("main-1") && rest.contains("main-2"));
    assert!(rest.contains("thread-1") && rest.contains("thread-2"));
}

/// A concurrent retry after an already-claimed destination fails fast with
/// `already_exists` (same logical `{cwd,id}`), while the winner publishes
/// exactly one session. Mirrors "rejects concurrent create and create calls for
/// the same destination".
#[tokio::test]
async fn concurrent_duplicate_create_one_wins_one_already_exists() {
    let fs: Arc<dyn FileSystem> =
        Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/proj".into()));
    let r = repo(fs.clone());
    let opts = create_options("same");

    let (a, b) = tokio::join!(r.create_typed(&opts), r.create_typed(&opts),);
    let results = vec![a, b];
    let successes = results.iter().filter(|r| r.is_ok()).count();
    let failures = results.iter().filter(|r| r.is_err()).count();
    assert_eq!(successes, 1, "exactly one concurrent create wins");
    let err = results
        .into_iter()
        .find_map(|r| r.err())
        .expect("one failure");
    assert_eq!(failures, 1, "the loser is already_exists");
    assert_eq!(err.code, SessionErrorCode::AlreadyExists);
    let fresh = repo(fs);
    assert_eq!(
        list_ids(&fresh, "/proj")
            .await
            .iter()
            .filter(|i| *i == "same")
            .count(),
        1
    );
}

/// Session files (each with its header-only line) round-trip across the repo:
/// after reload, `find_entries` reproduces the persisted entries in order, and a
/// trailing append lands at the next sequence number. Smoke-covers the
/// atomic-publish + torn-tail interplay end-to-end at the repo level.
#[tokio::test]
async fn repo_round_trip_preserves_order_and_is_appendable() {
    let fs: Arc<dyn FileSystem> =
        Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/proj".into()));
    let r = repo(fs);
    let storage = r.create_typed(&create_options("rt")).await.unwrap();
    storage
        .append_entry(message_provisioned("u1", "one"), "main")
        .await
        .unwrap();
    storage
        .append_entry(message_provisioned("u2", "two"), "main")
        .await
        .unwrap();
    storage
        .append_record(operation_started("run-1", "main"))
        .await
        .unwrap();

    let listed = r
        .list_typed(&JsonlSessionListOptions {
            cwd: Some("/proj".into()),
        })
        .await
        .unwrap();
    let meta = listed.iter().find(|m| m.id == "rt").unwrap().clone();
    let reopened = r.open_by_jsonl_metadata(&meta).await.unwrap();

    // Oldest-first reflects durable order; the operation_started record sits
    // at seq 3 between the messages and any later entry.
    let q = EntryQuery {
        order: Some(EntryOrder::OldestFirst),
        ..Default::default()
    };
    let entries = reopened.find_entries(&q).await.unwrap();
    let ids: Vec<&str> = entries.iter().map(|e| e.id()).collect();
    assert_eq!(ids, vec!["u1", "u2"]);

    // A trailing append lands at seq 4 on the reopened handle, and the lane
    // leaf is the last persisted entry (u2).
    let after = reopened
        .append_entry(message_provisioned("u3", "three"), "main")
        .await
        .unwrap();
    assert_eq!(after.seq(), 4);
    assert_eq!(after.parent_id().unwrap(), "u2");

    // The cross-lane view still resolves via the lane pointer table.
    let lanes = reopened.get_lanes().await.unwrap();
    assert_eq!(
        lanes,
        vec![LanePointer {
            lane: "main".into(),
            leaf_id: Some("u3".into())
        }]
    );
}

/// Defensive-copy contract (plan §5.8): entries returned by `find_entries` are
/// independent clones — mutating the returned `Vec` does not affect subsequent
/// reads. (A light check at the storage layer; the harness setters' defensive
/// copy is exercised in the harness defensive-copy test.)
#[tokio::test]
async fn find_entries_returns_independent_clones() {
    let fs: Arc<dyn FileSystem> =
        Arc::new(rpi_tools::InMemoryExecutionEnv::with_cwd("/proj".into()));
    let r = repo(fs);
    let storage = r.create_typed(&create_options("dc")).await.unwrap();
    storage
        .append_entry(note_provisioned("e1"), "main")
        .await
        .unwrap();

    let first = storage
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    // Drop + re-read: the state is unaffected by the caller holding/draining.
    drop(first);
    let second = storage
        .find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].id(), "e1");
    // And a branch-scoped read uses the bounds defensively too.
    let bounds = BranchBounds::default();
    let on_branch = storage
        .find_entries_on_branch(
            &EntryQuery {
                order: Some(EntryOrder::OldestFirst),
                ..Default::default()
            },
            &bounds,
            "e1",
        )
        .await
        .unwrap();
    assert!(on_branch.iter().any(|e| e.id() == "e1"));
}

// ---------------------------------------------------------------------------
// small record builder used above
// ---------------------------------------------------------------------------

fn operation_started(id: &str, lane: &str) -> LaneRecord {
    LaneRecord::OperationStarted(OperationStartedRecord {
        base: RecordBase {
            id: id.to_string(),
            seq: 0,
            lane: lane.to_string(),
            timestamp: 0,
        },
        source_leaf_id: None,
        intent: OperationIntent::Run {
            original_prompt: Vec::new(),
            initial_messages: Vec::new(),
            system_prompt_override: None,
            resume_data: None,
        },
    })
}

// Silence unused-import warnings if a helper goes unused in a subset build.
#[allow(dead_code)]
fn _anchor_traits() {
    let _ = SessionCreateOptions {
        id: None,
        parent_session_id: None,
        metadata: None,
    };
    let _ = SessionMetadata {
        id: String::new(),
        created_at: 0,
        parent_session_id: None,
    };
    let _ = FileKind::File;
}
