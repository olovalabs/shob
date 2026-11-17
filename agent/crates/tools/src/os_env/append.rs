//! Reusable append handles must follow the file currently named by the path.

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::read::OsReadFile;
use crate::env::{AppendFile, AppendReceipt, ReadFile};
use crate::error::{io_to_file_error, FileError, FileErrorCode};

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    volume: u64,
    index: u128,
}

#[cfg(windows)]
#[derive(Clone, Debug, Eq, PartialEq)]
struct FileIdentity(Arc<same_file::Handle>);

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity;

pub(super) struct OsAppendFile {
    path: PathBuf,
    state: Option<AppendState>,
    buffer: Vec<u8>,
    pending: Option<tokio::task::JoinHandle<AppendOutcome>>,
}

struct AppendState {
    file: Arc<Mutex<File>>,
    reader: Option<Arc<dyn ReadFile>>,
    identity: Option<FileIdentity>,
}

type AppendOutcome = (
    Option<AppendState>,
    Vec<u8>,
    io::Result<Option<AppendReceipt>>,
);

fn open_current(path: &Path) -> io::Result<AppendState> {
    if let Some(parent) = path.parent() {
        // Match append_file: the actual open decides whether the path is
        // writable, even if the caller cannot inspect an existing parent.
        let _ = std::fs::create_dir_all(parent);
    }
    // Read access is optional. Never require it for the legacy append path,
    // and never open a separate reader that might refer to a replaced path.
    let (file, readable) = match OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)
    {
        Ok(file) => (file, true),
        Err(_) => (
            OpenOptions::new().create(true).append(true).open(path)?,
            false,
        ),
    };
    // Some filesystems cannot provide stable IDs. Reopen on every append there,
    // preserving write-only access instead of requiring metadata permissions.
    let identity = identity_from_file(&file).ok();
    let file = Arc::new(Mutex::new(file));
    let reader = readable.then(|| {
        Arc::new(OsReadFile::from_shared(file.clone(), path.to_path_buf())) as Arc<dyn ReadFile>
    });
    Ok(AppendState {
        file,
        reader,
        identity,
    })
}

impl OsAppendFile {
    pub(super) async fn open(path: PathBuf) -> Result<Self, FileError> {
        let worker_path = path.clone();
        let state = tokio::task::spawn_blocking(move || open_current(&worker_path))
            .await
            .map_err(join_error)?
            .map_err(|error| io_to_file_error(error, Some(&path.to_string_lossy())))?;
        Ok(Self {
            path,
            state: Some(state),
            buffer: Vec::new(),
            pending: None,
        })
    }

    async fn finish_pending(&mut self) -> Result<Option<AppendReceipt>, FileError> {
        let Some(pending) = self.pending.as_mut() else {
            return Ok(None);
        };
        // Await by reference: cancellation must leave the task owned by this
        // writer. The next append joins it before starting another write.
        let outcome = pending.await;
        self.pending = None;
        let (state, mut buffer, result) = outcome.map_err(join_error)?;
        buffer.clear();
        // Do not retain a large image/tool payload for the lifetime of a session.
        self.buffer = if buffer.capacity() <= 64 * 1024 {
            buffer
        } else {
            Vec::new()
        };
        self.state = if result.is_ok() { state } else { None };
        result.map_err(|error| io_to_file_error(error, Some(&self.path.to_string_lossy())))
    }
}

fn join_error(error: tokio::task::JoinError) -> FileError {
    FileError::new(FileErrorCode::Unknown, error.to_string()).with_source(Box::new(error))
}

#[async_trait]
impl AppendFile for OsAppendFile {
    async fn append(&mut self, bytes: &[u8]) -> Result<(), FileError> {
        self.append_with_receipt(bytes).await.map(|_| ())
    }

    async fn append_with_receipt(
        &mut self,
        bytes: &[u8],
    ) -> Result<Option<AppendReceipt>, FileError> {
        // A cancelled caller's receipt is discarded, but its write must finish
        // (and any real error propagate) before a subsequent append can start.
        self.finish_pending().await?;
        let path = self.path.clone();
        let mut state = self.state.take();
        let mut buffer = std::mem::take(&mut self.buffer);
        buffer.extend_from_slice(bytes);
        self.pending = Some(tokio::task::spawn_blocking(move || {
            // Metadata, write and visibility flush share one blocking dispatch.
            // Splitting them into separate async fs operations wakes workers
            // repeatedly for every recovery frame.
            let result = (|| {
                let current = state.as_ref().and_then(|state| state.identity.clone());
                if current.is_none() || identity_at_path(&path).ok() != current {
                    state = Some(open_current(&path)?);
                }
                let state = state.as_ref().expect("current append file was opened");
                let mut file = state
                    .file
                    .lock()
                    .map_err(|_| io::Error::other("append mutex poisoned"))?;
                file.write_all(&buffer)?;
                file.flush()?;
                // Locate the range using the written handle, never its path.
                // Seeking to the end also handles empty appends and platforms
                // whose append writes do not advance the readable cursor.
                // Under the single-owner journal contract there is no other
                // writer interleaving the write_all calls. Receipt discovery
                // is best effort AFTER commit; a seek failure is not a failed
                // append and must never invite a duplicate retry.
                Ok(state.reader.as_ref().and_then(|reader| {
                    let length = u64::try_from(buffer.len()).ok()?;
                    let offset = file.seek(SeekFrom::End(0)).ok()?.checked_sub(length)?;
                    Some(AppendReceipt {
                        reader: reader.clone(),
                        offset,
                        length: buffer.len(),
                    })
                }))
            })();
            (state, buffer, result)
        }));
        self.finish_pending().await
    }
}

#[cfg(unix)]
fn identity_from_file(file: &File) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(FileIdentity {
        volume: metadata.dev(),
        index: metadata.ino().into(),
    })
}

#[cfg(unix)]
fn identity_at_path(path: &Path) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path)?;
    Ok(FileIdentity {
        volume: metadata.dev(),
        index: metadata.ino().into(),
    })
}

// Keep identity handles open while comparing them: Windows file IDs can be
// reused once handles close. The locked same-file crate exposes this safely.
#[cfg(windows)]
fn identity_from_file(file: &File) -> io::Result<FileIdentity> {
    same_file::Handle::from_file(file.try_clone()?).map(|handle| FileIdentity(Arc::new(handle)))
}

#[cfg(windows)]
fn identity_at_path(path: &Path) -> io::Result<FileIdentity> {
    use std::os::windows::fs::OpenOptionsExt;

    // File identity queries need metadata access only. Opening another data
    // reader after every write can force expensive Windows cache/oplock work.
    // access_mode(0) overrides GENERIC_READ, but still allows handle metadata
    // queries. Keep this handle alive through the same-file comparison so IDs
    // cannot be reused; never substitute pathname/stat heuristics for identity.
    let file = OpenOptions::new().read(true).access_mode(0).open(path)?;
    same_file::Handle::from_file(file).map(|handle| FileIdentity(Arc::new(handle)))
}

#[cfg(not(any(unix, windows)))]
fn identity_from_file(_file: &File) -> io::Result<FileIdentity> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(not(any(unix, windows)))]
fn identity_at_path(_path: &Path) -> io::Result<FileIdentity> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn metadata_identity_matches_open_file_without_reading_contents() {
        use std::io::Read;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal");
        std::fs::write(&path, b"synthetic contents").unwrap();
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .unwrap();
        let original = identity_from_file(&file).unwrap();
        let metadata = identity_at_path(&path).unwrap();
        assert_eq!(metadata, original);
        let mut metadata_file = metadata.0.as_file().try_clone().unwrap();
        assert_eq!(metadata_file.metadata().unwrap().len(), 18);
        assert!(
            metadata_file.read(&mut [0; 1]).is_err(),
            "identity handle must not read payloads"
        );
        let replacement = directory.path().join("replacement");
        std::fs::write(&replacement, b"different generation").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert_ne!(identity_at_path(&path).unwrap(), original);
        assert_eq!(
            metadata, original,
            "retired handles keep their original identity"
        );
    }

    #[tokio::test]
    async fn unknown_identity_reopens_after_external_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let mut writer = OsAppendFile::open(path.clone()).await.unwrap();
        writer.append(b"before\n").await.unwrap();
        writer.state.as_mut().unwrap().identity = None;
        let replacement = directory.path().join("replacement");
        std::fs::write(&replacement, b"copied\n").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        writer.append(b"after\n").await.unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"copied\nafter\n");
    }

    #[tokio::test]
    async fn missing_path_is_recreated_instead_of_writing_to_removed_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let backup = directory.path().join("backup");
        let mut writer = OsAppendFile::open(path.clone()).await.unwrap();
        writer.append(b"before\n").await.unwrap();
        std::fs::rename(&path, &backup).unwrap();
        writer.append(b"after\n").await.unwrap();
        assert_eq!(std::fs::read(backup).unwrap(), b"before\n");
        assert_eq!(std::fs::read(path).unwrap(), b"after\n");
    }

    #[tokio::test]
    async fn failed_reopen_does_not_acknowledge_a_write_to_the_old_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let backup = directory.path().join("backup");
        let mut writer = OsAppendFile::open(path.clone()).await.unwrap();
        writer.append(b"before\n").await.unwrap();
        std::fs::rename(&path, &backup).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(writer.append_with_receipt(b"after\n").await.is_err());
        assert_eq!(std::fs::read(backup).unwrap(), b"before\n");
    }

    #[tokio::test]
    async fn cancelled_append_wait_keeps_the_pending_writer_ordered() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let mut writer = OsAppendFile::open(path.clone()).await.unwrap();
        let state = writer.state.take().unwrap();
        let (release, wait) = tokio::sync::oneshot::channel();
        writer.pending = Some(tokio::spawn(async move {
            wait.await.unwrap();
            let result = state
                .file
                .lock()
                .unwrap()
                .write_all(b"pending\n")
                .map(|_| None);
            (Some(state), Vec::new(), result)
        }));
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(10),
            writer.append(b"cancelled\n")
        )
        .await
        .is_err());
        assert!(writer.pending.is_some());
        release.send(()).unwrap();
        let receipt = writer
            .append_with_receipt(b"next\n")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.offset, 8);
        assert_eq!(receipt.length, 5);
        assert_eq!(
            receipt
                .reader
                .read_range(receipt.offset, receipt.length)
                .await
                .unwrap(),
            b"next\n"
        );
        assert_eq!(std::fs::read(path).unwrap(), b"pending\nnext\n");
    }

    #[test]
    fn cancelled_receipt_wait_preserves_its_already_queued_append() {
        // One blocking worker makes cancellation deterministic without holding
        // a file lock across an await or relying on platform file identities.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("journal");
            let mut writer = OsAppendFile::open(path.clone()).await.unwrap();
            let (started, ready) = tokio::sync::oneshot::channel();
            let (release, wait) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = started.send(());
                let _ = wait.recv();
            });
            ready.await.unwrap();
            let cancelled = tokio::time::timeout(
                std::time::Duration::from_millis(10),
                writer.append_with_receipt(b"pending\n"),
            )
            .await
            .is_err();
            release.send(()).unwrap();
            blocker.await.unwrap();
            assert!(cancelled);
            assert!(writer.pending.is_some());
            let receipt = writer
                .append_with_receipt(b"next\n")
                .await
                .unwrap()
                .unwrap();
            assert_eq!((receipt.offset, receipt.length), (8, 5));
            assert_eq!(receipt.reader.read_range(8, 5).await.unwrap(), b"next\n");
            assert_eq!(std::fs::read(path).unwrap(), b"pending\nnext\n");
        });
    }

    #[tokio::test]
    async fn receipts_identify_written_ranges_and_share_the_opened_reader() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal");
        std::fs::write(&path, b"seed\n").unwrap();
        let mut writer = OsAppendFile::open(path.clone()).await.unwrap();
        let first = writer.append_with_receipt(b"one\n").await.unwrap().unwrap();
        let second = writer.append_with_receipt(b"two\n").await.unwrap().unwrap();
        assert_eq!((first.offset, first.length), (5, 4));
        assert_eq!((second.offset, second.length), (9, 4));
        // Mainstream OS backends share a reader across the whole generation;
        // otherwise every persisted cold entry would pin another file handle.
        #[cfg(any(unix, windows))]
        assert!(Arc::ptr_eq(&first.reader, &second.reader));
        let cloned = first.clone();
        assert!(Arc::ptr_eq(&cloned.reader, &first.reader));
        assert_eq!(cloned.reader.read_range(5, 4).await.unwrap(), b"one\n");
        assert_eq!(second.reader.read_range(9, 4).await.unwrap(), b"two\n");
        // A read moves the shared cursor; empty append still reports EOF.
        let empty = writer.append_with_receipt(b"").await.unwrap().unwrap();
        assert_eq!((empty.offset, empty.length), (13, 0));
        assert!(empty.reader.read_range(13, 0).await.unwrap().is_empty());
        let (read, appended) = tokio::join!(
            first.reader.read_range(first.offset, first.length),
            writer.append_with_receipt(b"three\n")
        );
        assert_eq!(read.unwrap(), b"one\n");
        let third = appended.unwrap().unwrap();
        assert_eq!((third.offset, third.length), (13, 6));
        assert_eq!(third.reader.byte_len().await.unwrap(), Some(19));
        assert_eq!(std::fs::read(path).unwrap(), b"seed\none\ntwo\nthree\n");
        assert!(third.reader.read_range(18, 2).await.is_err());
    }

    #[tokio::test]
    async fn receipts_survive_atomic_replacement_and_new_receipts_follow_the_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal");
        std::fs::write(&path, b"base\n").unwrap();
        let mut writer = OsAppendFile::open(path.clone()).await.unwrap();
        let old = writer.append_with_receipt(b"old\n").await.unwrap().unwrap();
        let replacement = directory.path().join("replacement");
        std::fs::write(&replacement, b"replacement\n").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let new = writer.append_with_receipt(b"new\n").await.unwrap().unwrap();
        assert!(!Arc::ptr_eq(&old.reader, &new.reader));
        assert_eq!(old.reader.byte_len().await.unwrap(), Some(9));
        assert_eq!(new.reader.byte_len().await.unwrap(), Some(16));
        assert_eq!((old.offset, old.length), (5, 4));
        assert_eq!((new.offset, new.length), (12, 4));
        drop(writer);
        assert_eq!(old.reader.read_all().await.unwrap(), b"base\nold\n");
        assert_eq!(
            old.reader.read_range(old.offset, old.length).await.unwrap(),
            b"old\n"
        );
        assert_eq!(
            new.reader.read_range(new.offset, new.length).await.unwrap(),
            b"new\n"
        );
        assert_eq!(new.reader.read_all().await.unwrap(), b"replacement\nnew\n");
    }

    #[tokio::test]
    async fn large_append_does_not_retain_its_buffer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal");
        let mut writer = OsAppendFile::open(path).await.unwrap();
        let bytes = vec![b'x'; 64 * 1024 + 1];
        writer.append_with_receipt(&bytes).await.unwrap();
        assert_eq!(writer.buffer.capacity(), 0);
        let receipt = writer.append_with_receipt(b"tail").await.unwrap().unwrap();
        assert_eq!(receipt.offset, bytes.len() as u64);
        assert!(writer.buffer.capacity() <= 64 * 1024);
        assert!(writer.buffer.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_only_permissions_do_not_require_receipts() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal");
        std::fs::write(&path, b"seed\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o200)).unwrap();
        // Privileged test users can bypass mode bits; no permission boundary
        // can be exercised in that environment.
        if OpenOptions::new().read(true).open(&path).is_ok() {
            return;
        }
        let mut writer = OsAppendFile::open(path.clone()).await.unwrap();
        assert!(writer.state.as_ref().unwrap().reader.is_none());
        assert!(writer
            .append_with_receipt(b"frame\n")
            .await
            .unwrap()
            .is_none());
        writer.append(b"legacy\n").await.unwrap();
        drop(writer);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"seed\nframe\nlegacy\n");
    }
}
