//! Storage-level capability and long-lived append regressions; no real files.
use super::*;
use crate::session::{
    jsonl::cold_frames::JsonlPayload,
    memory::{CounterIdGenerator, FakeClock},
    types::{
        AssistantFrameOp, AssistantFrameRecord, EntryOrder, ForkPosition, ProvisionedKind,
        RecordBase,
    },
};
use rpi_tools::{
    env::{AppendFile, AppendReceipt, FileInfo, ReadFile},
    FileError, FileErrorCode, InMemoryExecutionEnv,
};
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicUsize,
};
use tokio_util::sync::CancellationToken;

type FileResult<T> = Result<T, FileError>;

fn header() -> JsonlV4Header {
    JsonlV4Header::new("compat-test".into(), 1, "/cwd".into(), None, None, None)
}

#[derive(Clone, Copy)]
enum Mode {
    Stable,
    ReadAllOnly,
    LegacyRanged,
    RangeError,
    LengthError,
}
struct MemoryReader {
    bytes: std::sync::Mutex<Vec<u8>>,
    max_range: AtomicUsize,
    mode: Mode,
}
#[async_trait]
impl ReadFile for MemoryReader {
    async fn read_all(&self) -> FileResult<Vec<u8>> {
        panic!("stable JSONL history must not use read_all")
    }
    async fn byte_len(&self) -> FileResult<Option<u64>> {
        if matches!(self.mode, Mode::LengthError) {
            return Err(FileError::new(FileErrorCode::Unknown, "length I/O failure"));
        }
        Ok(Some(self.bytes.lock().unwrap().len() as u64))
    }
    async fn read_range(&self, offset: u64, length: usize) -> FileResult<Vec<u8>> {
        self.max_range.fetch_max(length, Ordering::Relaxed);
        let bytes = self.bytes.lock().unwrap();
        bytes
            .get(offset as usize..offset as usize + length)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| FileError::new(FileErrorCode::Unknown, "short range"))
    }
}
// Deliberately does not override byte_len: old reader implementations inherit None.
struct LegacyReader(Arc<MemoryReader>);
#[async_trait]
impl ReadFile for LegacyReader {
    async fn read_all(&self) -> FileResult<Vec<u8>> {
        Ok(self.0.bytes.lock().unwrap().clone())
    }
    async fn read_range(&self, offset: u64, length: usize) -> FileResult<Vec<u8>> {
        match self.0.mode {
            Mode::ReadAllOnly => Err(FileError::new(FileErrorCode::NotSupported, "read_all only")),
            Mode::RangeError => Err(FileError::new(FileErrorCode::Unknown, "range I/O failure")),
            _ => self.0.read_range(offset, length).await,
        }
    }
}
struct MemoryFs {
    inner: InMemoryExecutionEnv,
    mode: Mode,
    readers: std::sync::Mutex<HashMap<String, Arc<MemoryReader>>>,
}
impl MemoryFs {
    fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self {
            inner: InMemoryExecutionEnv::with_cwd("/".into()),
            mode,
            readers: Default::default(),
        })
    }
    fn reader(&self, path: &str) -> Arc<MemoryReader> {
        self.readers.lock().unwrap().get(path).unwrap().clone()
    }
}
struct MemoryAppend {
    fs: InMemoryExecutionEnv,
    path: String,
    reader: Arc<MemoryReader>,
}
#[async_trait]
impl AppendFile for MemoryAppend {
    async fn append(&mut self, bytes: &[u8]) -> FileResult<()> {
        self.append_with_receipt(bytes).await.map(|_| ())
    }
    async fn append_with_receipt(&mut self, bytes: &[u8]) -> FileResult<Option<AppendReceipt>> {
        self.fs
            .append_file(&self.path, FileContent::Bytes(bytes.to_vec()), None)
            .await?;
        let mut data = self.reader.bytes.lock().unwrap();
        let offset = data.len() as u64;
        data.extend_from_slice(bytes);
        Ok(Some(AppendReceipt {
            reader: self.reader.clone(),
            offset,
            length: bytes.len(),
        }))
    }
}
#[async_trait]
impl FileSystem for MemoryFs {
    fn cwd(&self) -> &Path {
        self.inner.cwd()
    }
    async fn absolute_path(
        &self,
        path: &str,
        c: Option<&CancellationToken>,
    ) -> FileResult<PathBuf> {
        self.inner.absolute_path(path, c).await
    }
    async fn join_path(
        &self,
        parts: &[&str],
        c: Option<&CancellationToken>,
    ) -> FileResult<PathBuf> {
        self.inner.join_path(parts, c).await
    }
    async fn read_text_file(
        &self,
        path: &str,
        c: Option<&CancellationToken>,
    ) -> FileResult<String> {
        self.inner.read_text_file(path, c).await
    }
    async fn read_text_lines(
        &self,
        path: &str,
        n: Option<usize>,
        c: Option<&CancellationToken>,
    ) -> FileResult<Vec<String>> {
        self.inner.read_text_lines(path, n, c).await
    }
    async fn read_binary_file(
        &self,
        path: &str,
        c: Option<&CancellationToken>,
    ) -> FileResult<Vec<u8>> {
        assert!(
            !matches!(self.mode, Mode::Stable),
            "stable failed append must only read its tail window"
        );
        self.inner.read_binary_file(path, c).await
    }
    async fn write_file(
        &self,
        path: &str,
        content: FileContent,
        c: Option<&CancellationToken>,
    ) -> FileResult<()> {
        self.inner.write_file(path, content, c).await?;
        let bytes = self.inner.read_binary_file(path, c).await?;
        self.readers.lock().unwrap().insert(
            path.into(),
            Arc::new(MemoryReader {
                bytes: std::sync::Mutex::new(bytes),
                max_range: AtomicUsize::new(0),
                mode: self.mode,
            }),
        );
        Ok(())
    }
    async fn append_file(
        &self,
        path: &str,
        content: FileContent,
        c: Option<&CancellationToken>,
    ) -> FileResult<()> {
        self.inner.append_file(path, content, c).await?;
        let bytes = self.inner.read_binary_file(path, c).await?;
        *self.reader(path).bytes.lock().unwrap() = bytes;
        Ok(())
    }
    async fn open_append_file(&self, path: &str) -> FileResult<Option<Box<dyn AppendFile>>> {
        if !matches!(self.mode, Mode::Stable) {
            return Ok(None);
        }
        Ok(Some(Box::new(MemoryAppend {
            fs: self.inner.clone(),
            path: path.into(),
            reader: self.reader(path),
        })))
    }
    async fn open_read_file(&self, path: &str) -> FileResult<Option<Arc<dyn ReadFile>>> {
        let reader = self.reader(path);
        Ok(Some(match self.mode {
            Mode::Stable | Mode::LengthError => reader as Arc<dyn ReadFile>,
            _ => Arc::new(LegacyReader(reader)) as Arc<dyn ReadFile>,
        }))
    }
    async fn truncate_file(
        &self,
        path: &str,
        len: u64,
        c: Option<&CancellationToken>,
    ) -> FileResult<()> {
        self.inner.truncate_file(path, len, c).await?;
        self.reader(path)
            .bytes
            .lock()
            .unwrap()
            .truncate(len as usize);
        Ok(())
    }
    async fn rename_file(
        &self,
        source: &str,
        dest: &str,
        c: Option<&CancellationToken>,
    ) -> FileResult<()> {
        self.inner.rename_file(source, dest, c).await?;
        let mut readers = self.readers.lock().unwrap();
        let reader = readers.remove(source).unwrap();
        readers.insert(dest.into(), reader);
        Ok(())
    }
    async fn file_info(&self, path: &str, c: Option<&CancellationToken>) -> FileResult<FileInfo> {
        self.inner.file_info(path, c).await
    }
    async fn list_dir(
        &self,
        path: &str,
        c: Option<&CancellationToken>,
    ) -> FileResult<Vec<FileInfo>> {
        self.inner.list_dir(path, c).await
    }
    async fn canonical_path(
        &self,
        path: &str,
        c: Option<&CancellationToken>,
    ) -> FileResult<PathBuf> {
        self.inner.canonical_path(path, c).await
    }
    async fn exists(&self, path: &str, c: Option<&CancellationToken>) -> FileResult<bool> {
        self.inner.exists(path, c).await
    }
    async fn create_dir(
        &self,
        path: &str,
        recursive: bool,
        c: Option<&CancellationToken>,
    ) -> FileResult<()> {
        self.inner.create_dir(path, recursive, c).await
    }
    async fn remove(
        &self,
        path: &str,
        recursive: bool,
        force: bool,
        c: Option<&CancellationToken>,
    ) -> FileResult<()> {
        self.inner.remove(path, recursive, force, c).await
    }
    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        c: Option<&CancellationToken>,
    ) -> FileResult<PathBuf> {
        self.inner.create_temp_dir(prefix, c).await
    }
    async fn create_temp_file(
        &self,
        prefix: &str,
        suffix: &str,
        c: Option<&CancellationToken>,
    ) -> FileResult<PathBuf> {
        self.inner.create_temp_file(prefix, suffix, c).await
    }
    async fn cleanup(&self) {
        FileSystem::cleanup(&self.inner).await;
    }
}
fn large_message(id: &str) -> ProvisionedEntry {
    ProvisionedEntry {
        id: id.into(),
        kind: ProvisionedKind::Message {
            message: rpi_ai::UserMessage::new("large payload 🌊".repeat(8192), 1).into(),
            terminate: None,
        },
    }
}
async fn create(fs: Arc<MemoryFs>) -> JsonlSessionStorage {
    JsonlSessionStorage::create(
        fs,
        "/history.jsonl",
        header(),
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    )
    .await
    .unwrap()
}
async fn load(fs: Arc<MemoryFs>) -> SessionResult<JsonlSessionStorage> {
    JsonlSessionStorage::load(
        fs,
        "/history.jsonl",
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    )
    .await
}
fn frame() -> LaneRecord {
    LaneRecord::AssistantFrame(AssistantFrameRecord {
        base: RecordBase {
            id: "legacy-frame".into(),
            seq: 0,
            lane: "main".into(),
            timestamp: 0,
        },
        run_id: "run".into(),
        stream_index: 0,
        op: AssistantFrameOp::Append,
        frame: Some(serde_json::json!({"delta":"legacy payload"})),
    })
}

#[tokio::test]
async fn read_all_only_reader_keeps_real_resident_entries_and_all_projections_work() {
    let fs = MemoryFs::new(Mode::ReadAllOnly);
    let original = create(fs.clone()).await;
    let mut entries = Vec::new();
    for id in ["e1", "e2", "e3"] {
        entries.push(
            original
                .append_entry(large_message(id), "main")
                .await
                .unwrap(),
        );
    }
    let record = original.append_record(frame()).await.unwrap();
    let storage = load(fs.clone()).await.unwrap();
    for entry in &entries {
        assert!(matches!(
            storage.state.lock().await.entry_ref(entry.id()),
            Some(JsonlPayload::Resident(_))
        ));
        assert_eq!(
            storage.get_entry(entry.id()).await.unwrap().as_ref(),
            Some(entry)
        );
    }
    assert_eq!(
        storage.find_entries(&EntryQuery::default()).await.unwrap(),
        original.find_entries(&EntryQuery::default()).await.unwrap()
    );
    assert_eq!(
        storage.find_records(&RecordQuery::default()).await.unwrap(),
        vec![record]
    );
    assert_eq!(
        storage
            .context_on_branch(&BranchBounds::default(), "e3")
            .await
            .unwrap()
            .messages,
        original
            .context_on_branch(&BranchBounds::default(), "e3")
            .await
            .unwrap()
            .messages
    );
    let fork = storage
        .fork(
            "/fork.jsonl",
            header(),
            &ForkOptions::Branch {
                entry_id: Some("e2".into()),
                position: Some(ForkPosition::At),
            },
            Arc::new(FakeClock::new()),
            Arc::new(CounterIdGenerator::new()),
        )
        .await
        .unwrap();
    assert_eq!(
        fork.find_entries(&EntryQuery {
            order: Some(EntryOrder::OldestFirst),
            ..Default::default()
        })
        .await
        .unwrap(),
        entries[..2]
    );
    assert_eq!(
        fork.get_entry("e1").await.unwrap().as_ref(),
        Some(&entries[0])
    );
}

#[tokio::test]
async fn legacy_range_capability_keeps_frames_cold_without_colding_entries() {
    let fs = MemoryFs::new(Mode::LegacyRanged);
    let original = create(fs.clone()).await;
    original
        .append_entry(large_message("e1"), "main")
        .await
        .unwrap();
    let expected = original.append_record(frame()).await.unwrap();
    let storage = load(fs.clone()).await.unwrap();
    assert!(matches!(
        storage.state.lock().await.entry_ref("e1"),
        Some(JsonlPayload::Resident(_))
    ));
    let reader = fs.reader("/history.jsonl");
    assert_eq!(
        reader.max_range.load(Ordering::Relaxed),
        0,
        "load should only probe range capability, not hydrate frames"
    );
    assert_eq!(
        storage.find_records(&RecordQuery::default()).await.unwrap(),
        vec![expected]
    );
    assert!(
        reader.max_range.load(Ordering::Relaxed) > 0,
        "legacy frame must remain cold"
    );
}

#[tokio::test]
async fn capability_io_errors_are_not_treated_as_absence() {
    for mode in [Mode::RangeError, Mode::LengthError] {
        let fs = MemoryFs::new(mode);
        create(fs.clone()).await;
        assert!(load(fs).await.is_err());
    }
}

struct TornAppend {
    fs: Arc<MemoryFs>,
    attempted: Arc<AtomicUsize>,
}
#[async_trait]
impl AppendFile for TornAppend {
    async fn append(&mut self, bytes: &[u8]) -> FileResult<()> {
        self.attempted.store(bytes.len(), Ordering::Relaxed);
        self.fs
            .append_file(
                "/history.jsonl",
                FileContent::Bytes(bytes[..bytes.len() / 2].to_vec()),
                None,
            )
            .await?;
        Err(FileError::new(FileErrorCode::Unknown, "short write"))
    }
}

#[tokio::test]
async fn many_large_successful_appends_share_one_reader_and_stay_cold_across_load_failure_retry() {
    let fs = MemoryFs::new(Mode::Stable);
    let storage = create(fs.clone()).await;
    let mut ids = Vec::new();
    let mut expected = Vec::new();
    let mut generation = None;
    for index in 0..48 {
        let id = format!("e{index}");
        expected.push(
            storage
                .append_entry(large_message(&id), "main")
                .await
                .unwrap(),
        );
        ids.push(id);
        let reader = storage.state.lock().await.assert_cold_shared(&ids);
        if let Some(first) = &generation {
            assert!(Arc::ptr_eq(first, &reader));
        } else {
            generation = Some(reader);
        }
    }
    let storage = load(fs.clone()).await.unwrap();
    let loaded_reader = storage.state.lock().await.assert_cold_shared(&ids);
    assert!(Arc::ptr_eq(generation.as_ref().unwrap(), &loaded_reader));
    expected.push(
        storage
            .append_entry(large_message("after-load"), "main")
            .await
            .unwrap(),
    );
    ids.push("after-load".into());
    assert!(Arc::ptr_eq(
        &loaded_reader,
        &storage.state.lock().await.assert_cold_shared(&ids)
    ));

    let reader = fs.reader("/history.jsonl");
    reader.max_range.store(0, Ordering::Relaxed);
    let attempted = Arc::new(AtomicUsize::new(0));
    storage.install_append_handle(Box::new(TornAppend {
        fs: fs.clone(),
        attempted: attempted.clone(),
    }));
    assert!(storage
        .append_entry(large_message("retry"), "main")
        .await
        .is_err());
    assert!(storage.get_entry("retry").await.unwrap().is_none());
    assert!(
        reader.max_range.load(Ordering::Relaxed) <= attempted.load(Ordering::Relaxed) + 1,
        "rollback may read only batch.len()+1, not history"
    );
    assert!(Arc::ptr_eq(
        &loaded_reader,
        &storage.state.lock().await.assert_cold_shared(&ids)
    ));
    for entry in &expected {
        assert_eq!(
            storage.get_entry(entry.id()).await.unwrap().as_ref(),
            Some(entry)
        );
    }
    expected.push(
        storage
            .append_entry(large_message("retry"), "main")
            .await
            .unwrap(),
    );
    ids.push("retry".into());
    assert!(Arc::ptr_eq(
        &loaded_reader,
        &storage.state.lock().await.assert_cold_shared(&ids)
    ));
    let reloaded = load(fs).await.unwrap();
    reloaded.state.lock().await.assert_cold_shared(&ids);
    for entry in &expected {
        assert_eq!(
            reloaded.get_entry(entry.id()).await.unwrap().as_ref(),
            Some(entry)
        );
    }
}

#[tokio::test]
async fn strict_and_recoverable_policies_match_on_streaming_and_eager_readers() {
    for mode in [Mode::Stable, Mode::ReadAllOnly, Mode::LegacyRanged] {
        for damage in [
            "none",
            "gap",
            "duplicate",
            "conflict",
            "zero",
            "torn",
            "torn-lf",
            "unterminated",
            "bad-frame",
            "schema",
        ] {
            let fs = MemoryFs::new(mode);
            let original = create(fs.clone()).await;
            original
                .append_entry(large_message("e1"), "main")
                .await
                .unwrap();
            let path = "/history.jsonl";
            let mut bytes = fs.read_text_file(path, None).await.unwrap();
            let row = bytes.lines().last().unwrap().to_owned();
            match damage {
                "gap" => bytes.push_str(&encode_mutation(&SessionMutation::FactName {
                    seq: 4,
                    name: Some("gap remains compatible".into()),
                })),
                "duplicate" => bytes.push_str(&format!("  {row} \n")),
                "conflict" => bytes.push_str(&encode_mutation(&SessionMutation::FactName {
                    seq: 1,
                    name: None,
                })),
                "zero" => bytes.push_str(&encode_mutation(&SessionMutation::FactName {
                    seq: 0,
                    name: None,
                })),
                "torn" => bytes.push_str("{unacknowledged"),
                "torn-lf" => bytes.push_str("{unacknowledged\n"),
                "unterminated" => {
                    bytes.pop();
                }
                "bad-frame" => bytes.push_str(
                    "{\"kind\":\"record\",\"type\":\"assistant_frame\",\"op\":\"append\"}\n",
                ),
                "schema" => bytes.push_str("{}\n"),
                _ => {}
            }
            fs.write_file(path, FileContent::Text(bytes.clone()), None)
                .await
                .unwrap();
            let progress = fs
                .read_text_file("/history.jsonl.progress.jsonl", None)
                .await
                .unwrap();
            for policy in [ReadPolicy::ResourceTransitionStrict, ReadPolicy::ReadOnly] {
                let result = JsonlSessionStorage::load_impl(
                    fs.clone(),
                    path,
                    Arc::new(FakeClock::new()),
                    Arc::new(CounterIdGenerator::new()),
                    policy,
                )
                .await;
                let expected = if policy.strict() {
                    matches!(damage, "none" | "gap")
                } else {
                    !matches!(damage, "conflict" | "zero" | "schema")
                };
                assert_eq!(
                    result.is_ok(),
                    expected,
                    "strict={}, damage={damage}",
                    policy.strict()
                );
                if let Ok(storage) = result {
                    assert!(storage.get_entry("e1").await.unwrap().is_some());
                    assert!(storage.facts().await.unwrap().values.is_empty());
                }
                assert_eq!(fs.read_text_file(path, None).await.unwrap(), bytes);
                assert_eq!(
                    fs.read_text_file("/history.jsonl.progress.jsonl", None)
                        .await
                        .unwrap(),
                    progress
                );
                assert_eq!(fs.list_dir("/", None).await.unwrap().len(), 2);
            }
            if !matches!(damage, "conflict" | "zero" | "schema") {
                let snapshot = JsonlSessionStorage::read_snapshot(
                    fs.clone(),
                    path,
                    Arc::new(FakeClock::new()),
                    Arc::new(CounterIdGenerator::new()),
                )
                .await
                .unwrap();
                // Snapshot materialization owns its bodies, not a lazy source capability.
                fs.write_file(path, FileContent::Text(String::new()), None)
                    .await
                    .unwrap();
                assert!(snapshot.get_entry("e1").await.unwrap().is_some());
            }
        }
    }
}

#[test]
fn rollback_window_never_invents_a_physical_line_boundary() {
    let batch = b"record\nnext\n";
    // Window begins in the middle of a foreign row, despite a matching suffix.
    assert_eq!(
        failed_append_start_at(b"record\nnext\n", batch, false),
        Some(batch.len())
    );
    assert_eq!(failed_append_start_at(b"record", batch, false), None);
    assert_eq!(failed_append_start_at(b"xrecord\nnext", batch, false), None);
    assert_eq!(
        failed_append_start_at(b"\nrecord\nnext", batch, false),
        Some(1)
    );
    assert_eq!(
        failed_append_start_at(b"\nrecord\nnext\n", batch, false),
        Some(1)
    );
    assert_eq!(failed_append_start_at(b"record", batch, true), Some(0));
}

#[path = "live_null_tests.rs"]
mod live_null_tests;
#[path = "storage/saved_read_tests.rs"]
mod saved_read_tests;
