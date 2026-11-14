//! Restricted native-owner filesystem: exactly two pinned regular inputs, no
//! writer, creation, directory enumeration, settings, environment or shell.
use async_trait::async_trait;
use rpi_tools::{
    env::ReadFile, FileContent, FileError, FileErrorCode, FileInfo, FileKind, FileSystem,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
const READ_LIMIT: usize = 64 * 1024 * 1024;
const RANGE_LIMIT: usize = 1024 * 1024;
const READ_CHUNK: usize = 64 * 1024;

/// Content hashes are always over entire sources, never over requested ranges.
/// Charge the aggregate source lengths once, including unrequested companions;
/// native scanners may reread the same page without consuming that budget again.
struct Budget {
    remaining: usize,
    deadline: Instant,
    hashes: BTreeMap<PathBuf, [u8; 32]>,
    sources: BTreeMap<PathBuf, SourceStamp>,
    absent: Option<PathBuf>,
}
impl Budget {
    fn check_deadline(&self) -> Result<(), FileError> {
        if Instant::now() >= self.deadline {
            return Err(failure("saved read processing deadline exceeded"));
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), FileError> {
        self.check_deadline()?;
        for (path, stamp) in &self.sources {
            if std::fs::canonicalize(path).map_err(io)? != *path
                || SourceStamp::new(&std::fs::symlink_metadata(path).map_err(io)?)? != *stamp
            {
                return Err(failure("saved read source changed"));
            }
        }
        if let Some(path) = &self.absent {
            match std::fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io(error)),
                Ok(_) => return Err(failure("saved read companion appeared")),
            }
        }
        self.check_deadline()
    }
}

/// Metadata detects generation changes, but is deliberately NOT the content
/// fingerprint. Unix pins device/inode and ctime as well as mtime (including
/// same-length writes whose mtime is restored). Windows holds a read-only-share
/// handle for the lifetime of the snapshot, denying writes/deletes/replacement.
#[derive(Clone, PartialEq, Eq)]
struct SourceStamp {
    length: u64,
    modified: std::time::SystemTime,
    created: Option<std::time::SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}
impl SourceStamp {
    fn new(meta: &std::fs::Metadata) -> Result<Self, FileError> {
        if !meta.file_type().is_file() {
            return Err(failure("saved reader refuses nonregular input"));
        }
        Ok(Self {
            length: meta.len(),
            modified: meta.modified().map_err(io)?,
            created: meta.created().ok(),
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec())
            },
        })
    }
}
fn failure(message: impl Into<String>) -> FileError {
    FileError::new(FileErrorCode::Invalid, message.into())
}
fn unsupported() -> FileError {
    FileError::new(
        FileErrorCode::NotSupported,
        "saved reader has no write/directory capability",
    )
}
fn io(error: std::io::Error) -> FileError {
    rpi_tools::io_to_file_error(error, None)
}
struct Input {
    // Retain the pinned generation (and Windows sharing restrictions).
    file: std::fs::File,
    path: PathBuf,
    stamp: SourceStamp,
    // Bounded raw bytes, not a decoded history/entry graph. Both eager and
    // streaming reads see exactly the bytes hashed during construction.
    bytes: Box<[u8]>,
    budget: Arc<Mutex<Budget>>,
}
impl Input {
    fn validate(&self) -> Result<(), FileError> {
        self.budget.lock().expect("saved read budget").validate()?;
        if SourceStamp::new(&self.file.metadata().map_err(io)?)? != self.stamp {
            return Err(failure("saved read opened source changed"));
        }
        Ok(())
    }

    fn read(&self, offset: u64, length: usize) -> Result<Vec<u8>, FileError> {
        if offset
            .checked_add(length as u64)
            .is_none_or(|end| end > self.bytes.len() as u64)
        {
            return Err(failure("saved read invalid source range"));
        }
        self.validate()?;
        let mut bytes = Vec::with_capacity(length);
        for chunk in self.bytes[offset as usize..offset as usize + length].chunks(READ_CHUNK) {
            self.budget
                .lock()
                .expect("saved read budget")
                .check_deadline()?;
            bytes.extend_from_slice(chunk);
        }
        self.validate()?;
        Ok(bytes)
    }
}
#[async_trait]
impl ReadFile for Input {
    async fn read_all(&self) -> Result<Vec<u8>, FileError> {
        self.read(0, self.bytes.len())
    }
    async fn read_range(&self, offset: u64, length: usize) -> Result<Vec<u8>, FileError> {
        if length > RANGE_LIMIT {
            return Err(failure("saved read range exceeds 1 MiB"));
        }
        self.read(offset, length)
    }
    async fn byte_len(&self) -> Result<Option<u64>, FileError> {
        self.validate()?;
        Ok(Some(self.bytes.len() as u64))
    }
}
pub(super) struct ReadonlyFs {
    cwd: PathBuf,
    inputs: BTreeMap<PathBuf, Arc<Input>>,
    budget: Arc<Mutex<Budget>>,
}
impl ReadonlyFs {
    pub(super) fn new(history: &Path, deadline: Instant) -> Result<Self, String> {
        Self::open(history, deadline).map_err(|error| error.to_string())
    }

    fn open(history: &Path, deadline: Instant) -> Result<Self, FileError> {
        let root = history
            .parent()
            .ok_or_else(|| failure("saved session has no parent directory"))?
            .to_owned();
        let progress = PathBuf::from(format!("{}.progress.jsonl", history.display()));
        let mut budget = Budget {
            remaining: READ_LIMIT,
            deadline,
            hashes: BTreeMap::new(),
            sources: BTreeMap::new(),
            absent: None,
        };
        let mut opened = Vec::new();
        // Reserve ALL input sizes before allocating or parsing any source. A
        // byte_len/range-only caller cannot bypass the aggregate 64 MiB bound.
        for path in [history.to_owned(), progress] {
            budget.check_deadline()?;
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && path != history => {
                    budget.absent = Some(path);
                    continue;
                }
                Err(e) => return Err(io(e)),
            };
            if !meta.file_type().is_file() || std::fs::canonicalize(&path).map_err(io)? != path {
                return Err(failure(
                    "saved reader refuses aliased/nonregular companion input",
                ));
            }
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options.share_mode(1); // FILE_SHARE_READ only: no write/delete.
            }
            let file = options.open(&path).map_err(io)?;
            let stamp = SourceStamp::new(&file.metadata().map_err(io)?)?;
            if stamp != SourceStamp::new(&std::fs::symlink_metadata(&path).map_err(io)?)? {
                return Err(failure("saved read source changed while opening"));
            }
            let length = usize::try_from(stamp.length)
                .map_err(|_| failure("saved read total byte limit exceeded"))?;
            budget.remaining = budget
                .remaining
                .checked_sub(length)
                .ok_or_else(|| failure("saved read total byte limit exceeded"))?;
            budget.sources.insert(path.clone(), stamp.clone());
            opened.push((path, file, stamp));
        }
        budget.validate()?;
        let budget = Arc::new(Mutex::new(budget));
        let mut inputs = BTreeMap::new();
        for (path, mut file, stamp) in opened {
            let mut bytes = vec![0; stamp.length as usize];
            let mut hash = Sha256::new();
            for chunk in bytes.chunks_mut(READ_CHUNK) {
                budget.lock().expect("saved read budget").check_deadline()?;
                file.read_exact(chunk).map_err(io)?;
                hash.update(chunk);
            }
            // Exact reads reject truncation; the stamp and EOF reject growth.
            let mut extra = [0];
            if file.read(&mut extra).map_err(io)? != 0 {
                return Err(failure("saved read source grew while reading"));
            }
            let input = Arc::new(Input {
                file,
                path: path.clone(),
                stamp,
                bytes: bytes.into_boxed_slice(),
                budget: budget.clone(),
            });
            input.validate()?;
            budget
                .lock()
                .expect("saved read budget")
                .hashes
                .insert(path.clone(), hash.finalize().into());
            inputs.insert(path, input);
        }
        budget.lock().expect("saved read budget").validate()?;
        Ok(Self {
            cwd: root,
            inputs,
            budget,
        })
    }
    pub(super) fn fingerprint(&self) -> String {
        let budget = self.budget.lock().expect("saved read budget");
        let mut hash = Sha256::new();
        for (path, digest) in &budget.hashes {
            let path = path.to_string_lossy();
            hash.update((path.len() as u64).to_le_bytes());
            hash.update(path.as_bytes());
            hash.update(digest);
        }
        format!("{:x}", hash.finalize())
    }
    fn input(&self, path: &str) -> Result<&Arc<Input>, FileError> {
        let input = self
            .inputs
            .get(Path::new(path))
            .ok_or_else(|| failure("saved reader input is missing or outside its fixed sources"))?;
        input.validate()?;
        Ok(input)
    }
}
#[async_trait]
impl FileSystem for ReadonlyFs {
    fn cwd(&self) -> &Path {
        &self.cwd
    }
    async fn absolute_path(
        &self,
        path: &str,
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            Ok(path)
        } else {
            Err(unsupported())
        }
    }
    async fn join_path(
        &self,
        _: &[&str],
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        Err(unsupported())
    }
    async fn read_text_file(
        &self,
        path: &str,
        _: Option<&CancellationToken>,
    ) -> Result<String, FileError> {
        String::from_utf8(self.input(path)?.read_all().await?)
            .map_err(|_| failure("invalid saved input UTF-8"))
    }
    async fn read_text_lines(
        &self,
        _: &str,
        _: Option<usize>,
        _: Option<&CancellationToken>,
    ) -> Result<Vec<String>, FileError> {
        Err(unsupported())
    }
    async fn read_binary_file(
        &self,
        path: &str,
        _: Option<&CancellationToken>,
    ) -> Result<Vec<u8>, FileError> {
        self.input(path)?.read_all().await
    }
    async fn open_read_file(&self, path: &str) -> Result<Option<Arc<dyn ReadFile>>, FileError> {
        Ok(Some(self.input(path)?.clone()))
    }
    async fn write_file(
        &self,
        _: &str,
        _: FileContent,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        Err(unsupported())
    }
    async fn append_file(
        &self,
        _: &str,
        _: FileContent,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        Err(unsupported())
    }
    async fn rename_file(
        &self,
        _: &str,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        Err(unsupported())
    }
    async fn file_info(
        &self,
        path: &str,
        _: Option<&CancellationToken>,
    ) -> Result<FileInfo, FileError> {
        let input = self.input(path)?;
        input.validate()?;
        let meta = input.file.metadata().map_err(io)?;
        Ok(FileInfo {
            name: input
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            path: input.path.clone(),
            kind: FileKind::File,
            size: meta.len(),
            mtime_ms: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
                .unwrap_or(0),
        })
    }
    async fn list_dir(
        &self,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<Vec<FileInfo>, FileError> {
        Err(unsupported())
    }
    async fn canonical_path(
        &self,
        path: &str,
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        Ok(self.input(path)?.path.clone())
    }
    async fn exists(&self, path: &str, _: Option<&CancellationToken>) -> Result<bool, FileError> {
        self.budget.lock().expect("saved read budget").validate()?;
        Ok(self.inputs.contains_key(Path::new(path)))
    }
    async fn create_dir(
        &self,
        _: &str,
        _: bool,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        Err(unsupported())
    }
    async fn remove(
        &self,
        _: &str,
        _: bool,
        _: bool,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        Err(unsupported())
    }
    async fn create_temp_dir(
        &self,
        _: Option<&str>,
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        Err(unsupported())
    }
    async fn create_temp_file(
        &self,
        _: &str,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        Err(unsupported())
    }
    async fn cleanup(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // Never open a real sessions tree: all sources, companions and aliases are
    // synthetic files under a fresh canonical temporary directory.
    fn fixture(history: &[u8], progress: Option<&[u8]>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("synthetic.jsonl");
        std::fs::write(&path, history).unwrap();
        if let Some(bytes) = progress {
            std::fs::write(companion(&path), bytes).unwrap();
        }
        (dir, path)
    }

    fn companion(path: &Path) -> PathBuf {
        PathBuf::from(format!("{}.progress.jsonl", path.display()))
    }

    fn open(path: &Path) -> ReadonlyFs {
        ReadonlyFs::new(path, Instant::now() + Duration::from_secs(60)).unwrap()
    }

    fn expected_fingerprint(sources: &[(PathBuf, &[u8])]) -> String {
        let mut sources = sources.to_vec();
        sources.sort_by(|a, b| a.0.cmp(&b.0));
        let mut hash = Sha256::new();
        for (path, bytes) in sources {
            let path = path.to_string_lossy();
            hash.update((path.len() as u64).to_le_bytes());
            hash.update(path.as_bytes());
            hash.update(Sha256::digest(bytes));
        }
        format!("{:x}", hash.finalize())
    }

    #[tokio::test]
    async fn eager_and_native_ranges_hash_complete_sources_once() {
        let history: Vec<_> = (0..RANGE_LIMIT + 37).map(|i| (i % 251) as u8).collect();
        let progress = b"synthetic progress, including unread tail\n";
        let (_dir, path) = fixture(&history, Some(progress));
        let eager = open(&path);
        let ranged = open(&path);
        let expected =
            expected_fingerprint(&[(path.clone(), &history), (companion(&path), progress)]);
        // Fingerprints include even sources/tails never touched by the caller.
        assert_eq!(ranged.fingerprint(), expected);
        assert_eq!(
            eager
                .read_binary_file(path.to_str().unwrap(), None)
                .await
                .unwrap(),
            history
        );
        let reader = ranged
            .open_read_file(path.to_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reader.byte_len().await.unwrap(), Some(history.len() as u64));
        assert_eq!(
            reader.read_range(RANGE_LIMIT as u64, 37).await.unwrap(),
            history[RANGE_LIMIT..]
        );
        assert_eq!(
            reader.read_range(0, RANGE_LIMIT).await.unwrap(),
            history[..RANGE_LIMIT]
        );
        let remaining = ranged.budget.lock().unwrap().remaining;
        // The total requested bytes exceed 64 MiB: these are rereads of the
        // same bounded snapshot, not additional source bytes or hash inputs.
        for _ in 0..65 {
            assert_eq!(
                reader.read_range(0, RANGE_LIMIT).await.unwrap(),
                history[..RANGE_LIMIT]
            );
        }
        assert_eq!(ranged.budget.lock().unwrap().remaining, remaining);
        assert_eq!(eager.fingerprint(), expected);
        assert_eq!(ranged.fingerprint(), expected);
        assert_eq!(reader.read_all().await.unwrap(), history);
        assert_eq!(ranged.fingerprint(), expected);
    }

    #[tokio::test]
    async fn range_bounds_empty_sources_and_utf8_are_not_silently_relaxed() {
        let (_dir, path) = fixture(b"\xffbroken JSON\n", Some(b""));
        let fs = open(&path);
        let reader = fs
            .open_read_file(path.to_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        let length = reader.byte_len().await.unwrap().unwrap();
        assert!(reader.read_range(0, RANGE_LIMIT + 1).await.is_err());
        assert!(reader.read_range(u64::MAX, 1).await.is_err());
        assert!(reader.read_range(u64::MAX, usize::MAX).await.is_err());
        assert!(reader.read_range(length, 1).await.is_err());
        assert!(reader.read_range(length + 1, 0).await.is_err());
        assert!(reader.read_range(length - 1, 2).await.is_err());
        assert_eq!(reader.read_range(length, 0).await.unwrap(), b"");
        assert_eq!(
            reader.read_range(0, length as usize).await.unwrap(),
            b"\xffbroken JSON\n"
        );
        assert!(fs
            .read_text_file(path.to_str().unwrap(), None)
            .await
            .is_err());
        // Range transport preserves malformed input for the storage layer's
        // strict ReadPolicy; it does not filter/repair/skip invalid bytes.
        assert_eq!(reader.read_all().await.unwrap(), b"\xffbroken JSON\n");
        let empty = fs
            .open_read_file(companion(&path).to_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(empty.byte_len().await.unwrap(), Some(0));
        assert_eq!(empty.read_all().await.unwrap(), b"");
        assert_eq!(empty.read_range(0, 0).await.unwrap(), b"");
        assert!(empty.read_range(0, 1).await.is_err());
    }

    #[tokio::test]
    async fn aggregate_source_budget_cannot_be_bypassed_by_tiny_ranges() {
        let (_dir, path) = fixture(b"", Some(b"x"));
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(READ_LIMIT as u64)
            .unwrap();
        // Individually legal lengths are still illegal in aggregate, before
        // any byte_len or range request has been made.
        assert!(
            ReadonlyFs::new(&path, Instant::now() + Duration::from_secs(60))
                .err()
                .unwrap()
                .contains("total byte limit")
        );
        std::fs::remove_file(companion(&path)).unwrap();
        let fs = open(&path); // exact 64 MiB is accepted
        assert_eq!(fs.budget.lock().unwrap().remaining, 0);
        let reader = fs
            .open_read_file(path.to_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reader.byte_len().await.unwrap(), Some(READ_LIMIT as u64));
        assert_eq!(
            reader.read_range(READ_LIMIT as u64 - 1, 1).await.unwrap(),
            [0]
        );
        assert_eq!(reader.read_range(0, 1).await.unwrap(), [0]);
        drop(reader);
        drop(fs);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(READ_LIMIT as u64 + 1)
            .unwrap();
        assert!(ReadonlyFs::new(&path, Instant::now() + Duration::from_secs(60)).is_err());
    }

    #[tokio::test]
    async fn deadlines_apply_to_length_zero_ranges_and_repeated_reads() {
        let (_dir, path) = fixture(b"synthetic", None);
        assert!(ReadonlyFs::new(&path, Instant::now() - Duration::from_secs(1)).is_err());
        let fs = open(&path);
        let reader = fs
            .open_read_file(path.to_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        fs.budget.lock().unwrap().deadline = Instant::now() - Duration::from_secs(1);
        assert!(reader.byte_len().await.is_err());
        assert!(reader.read_range(0, 0).await.is_err());
        assert!(reader.read_all().await.is_err());
        assert!(fs.open_read_file(path.to_str().unwrap()).await.is_err());
        assert!(fs.file_info(path.to_str().unwrap(), None).await.is_err());
        assert!(fs.exists(path.to_str().unwrap(), None).await.is_err());
    }

    #[tokio::test]
    async fn fingerprint_detects_content_changes_even_with_restored_mtime() {
        let (_dir, path) = fixture(b"old history", Some(b"old progress"));
        let mut previous = open(&path).fingerprint();
        for changed in [&path, &companion(&path)] {
            let mtime = std::fs::metadata(changed).unwrap().modified().unwrap();
            let mut bytes = std::fs::read(changed).unwrap();
            *bytes.last_mut().unwrap() ^= 1;
            std::fs::write(changed, bytes).unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(changed)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(mtime))
                .unwrap();
            let current = open(&path).fingerprint();
            assert_ne!(current, previous);
            previous = current;
        }
    }

    #[tokio::test]
    async fn an_absent_companion_is_pinned_as_absent() {
        let (_dir, path) = fixture(b"synthetic history", None);
        let fs = open(&path);
        let reader = fs
            .open_read_file(path.to_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        std::fs::write(companion(&path), b"appeared").unwrap();
        assert!(reader.byte_len().await.is_err());
        assert!(reader.read_range(0, 1).await.is_err());
        assert!(fs
            .exists(companion(&path).to_str().unwrap(), None)
            .await
            .is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn changes_to_either_source_fail_closed_for_both_read_paths() {
        for progress in [false, true] {
            for operation in ["truncate", "append", "rewrite", "replace", "remove"] {
                let (_dir, path) = fixture(b"old history", Some(b"old progress"));
                let fs = open(&path);
                let reader = fs
                    .open_read_file(path.to_str().unwrap())
                    .await
                    .unwrap()
                    .unwrap();
                let changed = if progress {
                    companion(&path)
                } else {
                    path.clone()
                };
                let meta = std::fs::metadata(&changed).unwrap();
                // Separate the snapshot and restored-mtime rewrite's ctime
                // ticks on the supported nanosecond-resolution Unix backends.
                if operation == "rewrite" {
                    std::thread::sleep(Duration::from_millis(20));
                }
                match operation {
                    "truncate" => std::fs::OpenOptions::new()
                        .write(true)
                        .open(&changed)
                        .unwrap()
                        .set_len(1)
                        .unwrap(),
                    "append" => {
                        use std::io::Write;
                        std::fs::OpenOptions::new()
                            .append(true)
                            .open(&changed)
                            .unwrap()
                            .write_all(b"+")
                            .unwrap();
                    }
                    "rewrite" => {
                        let mut bytes = std::fs::read(&changed).unwrap();
                        bytes[0] ^= 1;
                        std::fs::write(&changed, bytes).unwrap();
                        std::fs::OpenOptions::new()
                            .write(true)
                            .open(&changed)
                            .unwrap()
                            .set_times(
                                std::fs::FileTimes::new().set_modified(meta.modified().unwrap()),
                            )
                            .unwrap();
                    }
                    "replace" => {
                        let replacement = changed.with_extension("replacement");
                        std::fs::write(&replacement, std::fs::read(&changed).unwrap()).unwrap();
                        std::fs::File::open(&replacement)
                            .unwrap()
                            .set_times(
                                std::fs::FileTimes::new().set_modified(meta.modified().unwrap()),
                            )
                            .unwrap();
                        std::fs::rename(replacement, &changed).unwrap();
                    }
                    "remove" => std::fs::remove_file(&changed).unwrap(),
                    _ => unreachable!(),
                }
                assert!(
                    reader.byte_len().await.is_err(),
                    "{operation}, progress={progress}"
                );
                assert!(
                    reader.read_range(0, 1).await.is_err(),
                    "{operation}, progress={progress}"
                );
                assert!(
                    reader.read_all().await.is_err(),
                    "{operation}, progress={progress}"
                );
                assert!(fs.file_info(path.to_str().unwrap(), None).await.is_err());
            }
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_pins_deny_writers_and_replacement_until_dropped() {
        let (_dir, path) = fixture(b"synthetic history", Some(b"synthetic progress"));
        let fs = open(&path);
        for source in [&path, &companion(&path)] {
            assert!(std::fs::OpenOptions::new()
                .write(true)
                .open(source)
                .is_err());
            assert!(std::fs::remove_file(source).is_err());
            assert!(std::fs::rename(source, source.with_extension("renamed")).is_err());
        }
        let reader = fs
            .open_read_file(path.to_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reader.read_range(0, 9).await.unwrap(), b"synthetic");
        drop(reader);
        drop(fs);
        std::fs::write(&path, b"now writable").unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn aliases_and_nonregular_companions_are_rejected() {
        use std::os::unix::fs::symlink;
        let (_dir, path) = fixture(b"synthetic history", None);
        let alias = path.with_extension("alias");
        symlink(&path, &alias).unwrap();
        assert!(ReadonlyFs::new(&alias, Instant::now() + Duration::from_secs(60)).is_err());
        symlink(&path, companion(&path)).unwrap();
        assert!(ReadonlyFs::new(&path, Instant::now() + Duration::from_secs(60)).is_err());
        std::fs::remove_file(companion(&path)).unwrap();
        std::fs::create_dir(companion(&path)).unwrap();
        assert!(ReadonlyFs::new(&path, Instant::now() + Duration::from_secs(60)).is_err());
    }

    #[tokio::test]
    async fn read_only_capabilities_never_create_repair_or_escape() {
        let (dir, path) = fixture(b"malformed synthetic input", None);
        let fs = open(&path);
        let addressed = path.to_str().unwrap();
        let outside = path.with_extension("unregistered");
        std::fs::write(&outside, b"not an authorized input").unwrap();
        assert!(fs.open_read_file(outside.to_str().unwrap()).await.is_err());
        assert!(fs
            .read_binary_file(outside.to_str().unwrap(), None)
            .await
            .is_err());
        assert!(fs
            .write_file(addressed, FileContent::Bytes(vec![]), None)
            .await
            .is_err());
        assert!(fs
            .append_file(addressed, FileContent::Bytes(vec![]), None)
            .await
            .is_err());
        assert!(fs
            .rename_file(addressed, outside.to_str().unwrap(), None)
            .await
            .is_err());
        assert!(fs.remove(addressed, false, false, None).await.is_err());
        assert!(fs
            .create_dir(outside.to_str().unwrap(), true, None)
            .await
            .is_err());
        assert!(fs
            .create_temp_file("synthetic", ".tmp", None)
            .await
            .is_err());
        assert!(fs.create_temp_dir(None, None).await.is_err());
        assert!(fs
            .list_dir(dir.path().to_str().unwrap(), None)
            .await
            .is_err());
        fs.cleanup().await;
        assert_eq!(std::fs::read(&path).unwrap(), b"malformed synthetic input");
        assert_eq!(std::fs::read(&outside).unwrap(), b"not an authorized input");
        assert!(!companion(&path).exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }
}
