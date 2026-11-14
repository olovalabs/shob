use super::*;
use crate::{env::FileSystem, os_env::OsExecutionEnv};
use std::{
    io::Cursor,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll},
};
use tokio::io::{AsyncWriteExt, ReadBuf};

struct CountingReader {
    bytes: Cursor<Vec<u8>>,
    read: Arc<AtomicUsize>,
    cancel_after_read: Option<CancellationToken>,
}

impl AsyncRead for CountingReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let start = self.bytes.position() as usize;
        let count = buffer.remaining().min(self.bytes.get_ref().len() - start);
        buffer.put_slice(&self.bytes.get_ref()[start..start + count]);
        self.bytes.set_position((start + count) as u64);
        self.read.fetch_add(count, Ordering::SeqCst);
        if let Some(token) = &self.cancel_after_read {
            token.cancel();
        }
        Poll::Ready(Ok(()))
    }
}

fn counted(bytes: Vec<u8>) -> (CountingReader, Arc<AtomicUsize>) {
    let read = Arc::new(AtomicUsize::new(0));
    (
        CountingReader {
            bytes: Cursor::new(bytes),
            read: read.clone(),
            cancel_after_read: None,
        },
        read,
    )
}

fn old_lines(text: &str, max: Option<usize>) -> Vec<String> {
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    if let Some(max) = max {
        lines.truncate(max);
    }
    lines
}

#[tokio::test]
async fn preserves_split_semantics_and_limits() {
    for text in [
        "",
        "one",
        "one\n",
        "\n",
        "\n\n",
        "a\n\nb",
        "a\r\nb\r\n",
        "\r",
    ] {
        for max in [None, Some(0), Some(1), Some(2), Some(3), Some(usize::MAX)] {
            let actual = read_lines_from(text.as_bytes(), "test.jsonl", max, None)
                .await
                .unwrap();
            assert_eq!(actual, old_lines(text, max), "text={text:?}, max={max:?}");
        }
    }
}

#[tokio::test]
async fn multibyte_characters_cross_the_buffer_boundary() {
    let first = format!("{}中", "a".repeat(LINE_READ_BUFFER_SIZE - 1));
    let text = format!("{first}\n🙂\r\nlast");
    for max in [None, Some(1), Some(2)] {
        let lines = read_lines_from(text.as_bytes(), "unicode.jsonl", max, None)
            .await
            .unwrap();
        assert_eq!(lines, old_lines(&text, max));
    }
}

#[tokio::test]
async fn first_line_reads_only_one_buffer_and_does_not_validate_buffered_tail() {
    let mut bytes = b"header\n".to_vec();
    // Invalid UTF-8 starts in the very same buffer as the selected line.
    bytes.resize(LINE_READ_BUFFER_SIZE * 4, 0xff);
    let (reader, read) = counted(bytes);
    assert_eq!(
        read_lines_from(reader, "history.jsonl", Some(1), None)
            .await
            .unwrap(),
        ["header"]
    );
    assert_eq!(read.load(Ordering::SeqCst), LINE_READ_BUFFER_SIZE);
}

#[tokio::test]
async fn selected_invalid_utf8_is_invalid_with_path_including_at_eof() {
    for bytes in [b"ok\n\xff\n".as_slice(), b"ok\n\xe4\xb8".as_slice()] {
        for max in [None, Some(2)] {
            let error = read_lines_from(bytes, "invalid.jsonl", max, None)
                .await
                .unwrap_err();
            assert_eq!(error.code, FileErrorCode::Invalid);
            assert_eq!(error.path.as_deref(), Some("invalid.jsonl"));
        }
    }
}

#[tokio::test]
async fn file_prefix_ignores_invalid_tail_but_full_reads_still_reject_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.jsonl");
    std::fs::write(&path, b"header\n\xff\n").unwrap();
    let env = OsExecutionEnv::with_cwd(dir.path().to_owned());
    assert_eq!(
        env.read_text_lines("history.jsonl", Some(1), None)
            .await
            .unwrap(),
        ["header"]
    );
    let addressed = path.to_string_lossy();
    for error in [
        env.read_text_lines("history.jsonl", None, None)
            .await
            .unwrap_err(),
        env.read_text_file("history.jsonl", None).await.unwrap_err(),
    ] {
        assert_eq!(error.code, FileErrorCode::Invalid);
        assert_eq!(error.path.as_deref(), Some(addressed.as_ref()));
    }
}

#[tokio::test]
async fn zero_does_not_open_or_read_but_checks_cancellation_first() {
    let dir = tempfile::tempdir().unwrap();
    let env = OsExecutionEnv::with_cwd(dir.path().to_owned());
    assert!(env
        .read_text_lines("missing.jsonl", Some(0), None)
        .await
        .unwrap()
        .is_empty());
    let (reader, read) = counted(b"not read".to_vec());
    assert!(read_lines_from(reader, "test", Some(0), None)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(read.load(Ordering::SeqCst), 0);
    let cancel = CancellationToken::new();
    cancel.cancel();
    for max in [Some(0), Some(1), None] {
        let error = env
            .read_text_lines("missing.jsonl", max, Some(&cancel))
            .await
            .unwrap_err();
        assert_eq!(error.code, FileErrorCode::Aborted);
        assert_eq!(error.path.as_deref(), Some("missing.jsonl"));
    }
}

#[tokio::test]
async fn open_io_error_has_the_addressed_path() {
    let dir = tempfile::tempdir().unwrap();
    let env = OsExecutionEnv::with_cwd(dir.path().to_owned());
    let error = env
        .read_text_lines("missing.jsonl", Some(1), None)
        .await
        .unwrap_err();
    assert_eq!(error.code, FileErrorCode::NotFound);
    assert_eq!(
        error.path.as_deref(),
        Some(dir.path().join("missing.jsonl").to_string_lossy().as_ref())
    );
}

#[tokio::test]
async fn pending_open_operation_is_cancellable() {
    let cancel = CancellationToken::new();
    // This is the same wrapper used for File::open; no platform-specific FIFO
    // or blocking filesystem operation is needed to observe an opening wait.
    let operation = line_read_io(
        std::future::pending::<std::io::Result<()>>(),
        "opening.jsonl",
        Some(&cancel),
    );
    tokio::pin!(operation);
    assert!(futures::poll!(operation.as_mut()).is_pending());
    cancel.cancel();
    let error = operation.await.unwrap_err();
    assert_eq!(error.code, FileErrorCode::Aborted);
    assert_eq!(error.path.as_deref(), Some("opening.jsonl"));
}

#[tokio::test]
async fn pending_read_is_cancellable_after_partial_line_progress() {
    let cancel = CancellationToken::new();
    let (mut writer, reader) = tokio::io::duplex(64);
    writer.write_all(b"unfinished line").await.unwrap();
    let operation = read_lines_from(reader, "pending.jsonl", Some(1), Some(&cancel));
    tokio::pin!(operation);
    assert!(futures::poll!(operation.as_mut()).is_pending());
    cancel.cancel();
    let error = operation.await.unwrap_err();
    assert_eq!(error.code, FileErrorCode::Aborted);
    assert_eq!(error.path.as_deref(), Some("pending.jsonl"));
}

#[tokio::test]
async fn long_line_checks_cancellation_between_bounded_chunks() {
    let cancel = CancellationToken::new();
    let (mut reader, read) = counted(vec![b'a'; LINE_READ_BUFFER_SIZE * 4]);
    reader.cancel_after_read = Some(cancel.clone());
    let error = read_lines_from(reader, "long.jsonl", Some(1), Some(&cancel))
        .await
        .unwrap_err();
    assert_eq!(error.code, FileErrorCode::Aborted);
    assert_eq!(error.path.as_deref(), Some("long.jsonl"));
    assert_eq!(read.load(Ordering::SeqCst), LINE_READ_BUFFER_SIZE);
}

struct BrokenReader;
impl AsyncRead for BrokenReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "test read failure",
        )))
    }
}

#[tokio::test]
async fn read_io_error_is_mapped_with_path() {
    let error = read_lines_from(BrokenReader, "broken.jsonl", Some(1), None)
        .await
        .unwrap_err();
    assert_eq!(error.code, FileErrorCode::PermissionDenied);
    assert_eq!(error.path.as_deref(), Some("broken.jsonl"));
}
