//! A file-generation reader; unlike append handles it never follows a rename.
use crate::{
    env::{check_cancel_file, ReadFile},
    error::{io_to_file_error, FileError, FileErrorCode},
};
use async_trait::async_trait;
use std::{
    fs::{File, OpenOptions},
    future::Future,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio_util::sync::CancellationToken;

const LINE_READ_BUFFER_SIZE: usize = 8 * 1024;

/// Read only the requested prefix. Buffered read-ahead is bounded to 8 KiB;
/// bytes after the selected newline are neither consumed nor UTF-8 validated.
/// Memory beyond the buffer is proportional to the selected lines (a single
/// selected line can itself be arbitrarily long).
pub(super) async fn read_text_lines(
    path: &Path,
    max_lines: Option<usize>,
    cancel: Option<&CancellationToken>,
) -> Result<Vec<String>, FileError> {
    let addressed = path.to_string_lossy();
    check_cancel_file(cancel, Some(&addressed))?;
    if max_lines == Some(0) {
        return Ok(Vec::new());
    }
    let file = line_read_io(tokio::fs::File::open(path), &addressed, cancel).await?;
    read_lines_from(file, &addressed, max_lines, cancel).await
}

async fn line_read_io<T>(
    operation: impl Future<Output = std::io::Result<T>>,
    path: &str,
    cancel: Option<&CancellationToken>,
) -> Result<T, FileError> {
    check_cancel_file(cancel, Some(path))?;
    let result = if let Some(token) = cancel {
        tokio::select! {
            biased;
            _ = token.cancelled() => return Err(FileError::aborted().with_path(path)),
            result = operation => result,
        }
    } else {
        operation.await
    };
    result.map_err(|error| io_to_file_error(error, Some(path)))
}

async fn read_lines_from<R: AsyncRead + Unpin>(
    reader: R,
    path: &str,
    max_lines: Option<usize>,
    cancel: Option<&CancellationToken>,
) -> Result<Vec<String>, FileError> {
    check_cancel_file(cancel, Some(path))?;
    if max_lines == Some(0) {
        return Ok(Vec::new());
    }
    let mut reader = BufReader::with_capacity(LINE_READ_BUFFER_SIZE, reader);
    let mut lines = Vec::new();
    let mut line = Vec::new();
    loop {
        // Check cancellation on every bounded chunk, including long lines.
        let available = line_read_io(reader.fill_buf(), path, cancel).await?;
        if available.is_empty() {
            // split('\n') historically returned [""] for an empty file, but
            // did not retain the extra empty item after a trailing newline.
            if !line.is_empty() || lines.is_empty() {
                lines.push(decode_line(line, path)?);
            }
            return Ok(lines);
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |index| index + 1);
        let content_len = if newline.is_some() { count - 1 } else { count };
        line.extend_from_slice(&available[..content_len]);
        reader.consume(count);
        if newline.is_some() {
            lines.push(decode_line(std::mem::take(&mut line), path)?);
            if max_lines == Some(lines.len()) {
                return Ok(lines);
            }
        }
    }
}

fn decode_line(bytes: Vec<u8>, path: &str) -> Result<String, FileError> {
    String::from_utf8(bytes).map_err(|error| {
        FileError::new(FileErrorCode::Invalid, format!("invalid utf-8: {error}")).with_path(path)
    })
}

pub(super) struct OsReadFile {
    file: Arc<Mutex<File>>,
    path: PathBuf,
}
impl OsReadFile {
    pub(super) async fn open(path: PathBuf) -> Result<Self, FileError> {
        let addressed = path.clone();
        let file = tokio::task::spawn_blocking(move || {
            // std's Windows default shares read/write/delete, permitting the
            // journal's existing atomic replacement while readers keep a view.
            OpenOptions::new().read(true).open(addressed)
        })
        .await
        .map_err(join_error)?
        .map_err(|error| io_to_file_error(error, Some(&path.to_string_lossy())))?;
        Ok(Self::from_shared(Arc::new(Mutex::new(file)), path))
    }
    /// Share the append handle itself, not a second open of its pathname.
    /// Reads and writes must hold this mutex for all cursor operations.
    pub(super) fn from_shared(file: Arc<Mutex<File>>, path: PathBuf) -> Self {
        Self { file, path }
    }
    async fn read(&self, range: Option<(u64, usize)>) -> Result<Vec<u8>, FileError> {
        let shared = self.file.clone();
        tokio::task::spawn_blocking(move || {
            let mut file = shared
                .lock()
                .map_err(|_| std::io::Error::other("reader mutex poisoned"))?;
            let mut bytes = Vec::new();
            match range {
                Some((offset, length)) => {
                    file.seek(SeekFrom::Start(offset))?;
                    bytes.resize(length, 0);
                    file.read_exact(&mut bytes)?;
                }
                None => {
                    file.seek(SeekFrom::Start(0))?;
                    file.read_to_end(&mut bytes)?;
                }
            }
            Ok(bytes)
        })
        .await
        .map_err(join_error)?
        .map_err(|error| io_to_file_error(error, Some(&self.path.to_string_lossy())))
    }
}
fn join_error(error: tokio::task::JoinError) -> FileError {
    FileError::new(FileErrorCode::Unknown, error.to_string()).with_source(Box::new(error))
}
#[async_trait]
impl ReadFile for OsReadFile {
    async fn read_all(&self) -> Result<Vec<u8>, FileError> {
        self.read(None).await
    }
    async fn read_range(&self, offset: u64, length: usize) -> Result<Vec<u8>, FileError> {
        self.read(Some((offset, length))).await
    }
    async fn byte_len(&self) -> Result<Option<u64>, FileError> {
        let shared = self.file.clone();
        tokio::task::spawn_blocking(move || {
            let file = shared
                .lock()
                .map_err(|_| std::io::Error::other("reader mutex poisoned"))?;
            file.metadata().map(|metadata| Some(metadata.len()))
        })
        .await
        .map_err(join_error)?
        .map_err(|error| io_to_file_error(error, Some(&self.path.to_string_lossy())))
    }
}

#[cfg(test)]
#[path = "read_lines_tests.rs"]
mod read_lines_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_handle_keeps_its_generation_after_native_rename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        std::fs::write(&path, b"old-generation").unwrap();
        let reader = OsReadFile::open(path.clone()).await.unwrap();
        assert_eq!(reader.byte_len().await.unwrap(), Some(14));
        let old = dir.path().join("old.jsonl");
        std::fs::rename(&path, &old).unwrap();
        std::fs::write(&path, b"new").unwrap();
        assert_eq!(reader.byte_len().await.unwrap(), Some(14));
        assert_eq!(reader.read_all().await.unwrap(), b"old-generation");
        let (left, right) = tokio::join!(reader.read_range(0, 3), reader.read_range(4, 10));
        assert_eq!(left.unwrap(), b"old");
        assert_eq!(right.unwrap(), b"generation");
        assert!(reader.read_range(10, 100).await.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        // Length tracks the opened generation, not a cached opening length or
        // the path now naming another file.
        OpenOptions::new()
            .write(true)
            .open(&old)
            .unwrap()
            .set_len(3)
            .unwrap();
        assert_eq!(reader.byte_len().await.unwrap(), Some(3));
        assert_eq!(reader.read_all().await.unwrap(), b"old");
        assert!(reader.read_range(0, 4).await.is_err());
    }
}
