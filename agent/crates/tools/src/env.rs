//! Mirrors `packages/agent/src/harness/types.ts::ExecutionEnv` — the trait every
//! tool and test-double implements. `ExecutionEnv = FileSystem + Shell`.
//!
//! Contract (plan §5.13): **every method must never panic.** All failures are
//! encoded in `Result<_, FileError>` / `Result<_, ExecutionError>`. Paths may be
//! absolute or relative to [`FileSystem::cwd`]; returned paths are "addressed"
//! (symlink-canonicalized only by [`FileSystem::canonical_path`]).
//!
//! The TS `AbortSignal` becomes `&tokio_util::sync::CancellationToken` on every
//! method. Backends SHOULD check `is_cancelled()` at entry and honor cooperative
//! cancellation during blocking ops via `tokio::select!`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::error::{ExecutionError, FileError, FileErrorCode};

/// `file` | `directory` | `symlink`. Mirrors TS `FileKind`.
///
/// Backends detect via `symlink_metadata` (lstat) — symlinks are NOT followed
/// automatically by any `FileSystem` method except `canonical_path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
}

/// Mirrors TS `FileInfo`. `path` is the absolute addressed (resolved) path —
/// NOT symlink-canonicalized. `mtime_ms` is ms since Unix epoch.
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub name: String,
    pub path: PathBuf,
    pub kind: FileKind,
    pub size: u64,
    pub mtime_ms: i64,
}

/// Write/append payload. Mirrors the TS `string | Uint8Array` union.
#[derive(Debug, Clone)]
pub enum FileContent {
    Text(String),
    Bytes(Vec<u8>),
}

impl FileContent {
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            FileContent::Text(s) => s.as_bytes(),
            FileContent::Bytes(b) => b,
        }
    }
}

impl From<String> for FileContent {
    fn from(s: String) -> Self {
        FileContent::Text(s)
    }
}

impl From<&str> for FileContent {
    fn from(s: &str) -> Self {
        FileContent::Text(s.to_string())
    }
}

impl From<Vec<u8>> for FileContent {
    fn from(b: Vec<u8>) -> Self {
        FileContent::Bytes(b)
    }
}

/// Result of `Shell::exec`. Mirrors the TS inline return type
/// `{ stdout, stderr, exitCode }`. Signal kills collapse to `exit_code: 0`
/// (matching the TS `code ?? 0` rule).
#[derive(Debug, Clone)]
pub struct ShellOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Mirrors TS `ShellExecOptions` (note: `timeout` is in **seconds**, matching
/// the TS contract). `on_stdout` / `on_stderr` fire per chunk, interleaved in
/// arrival order; the bash-tool wraps these with throttling + capture.
#[derive(Clone, Debug)]
pub struct ShellOverride {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// A decoded output chunk. Bounded channels let streaming consumers apply
/// asynchronous backpressure without blocking the runtime's pipe readers.
pub enum ShellOutputChunk {
    Stdout(String),
    Stderr(String),
}

pub struct ShellExecOptions<'a> {
    /// Optional executable and fixed arguments. `None` keeps the environment's
    /// default shell (Bash on POSIX/Windows), while tools such as PowerShell
    /// can select their native interpreter without going through Bash.
    pub shell: Option<ShellOverride>,
    pub cwd: Option<PathBuf>,
    pub env: Option<HashMap<String, String>>,
    /// Default `true`. When `false`, ONLY `env` is used (no process env, no
    /// shell base env).
    pub inherit_env: bool,
    /// Seconds. `None` = no timeout.
    pub timeout: Option<f64>,
    pub cancel: Option<&'a CancellationToken>,
    pub on_stdout: Option<Box<dyn FnMut(&str) + Send + 'a>>,
    pub on_stderr: Option<Box<dyn FnMut(&str) + Send + 'a>>,
    /// Optional bounded asynchronous output transport, in addition to callbacks.
    /// Consumers must drain it concurrently, including final decoder tails.
    /// Backpressure can delay final delivery but does not delay child-tree kills.
    pub output: Option<tokio::sync::mpsc::Sender<ShellOutputChunk>>,
    /// Default `true`. Callers that already consume the stream through
    /// `on_stdout`/`on_stderr` (for example the bash capture layer, which keeps
    /// its own bounded tail) can set this to `false` to avoid retaining a second
    /// unbounded copy of the output.
    pub capture_output: bool,
}

impl<'a> Default for ShellExecOptions<'a> {
    fn default() -> Self {
        Self {
            shell: None,
            cwd: None,
            env: None,
            inherit_env: true,
            timeout: None,
            cancel: None,
            on_stdout: None,
            on_stderr: None,
            output: None,
            capture_output: true,
        }
    }
}

impl<'a> ShellExecOptions<'a> {
    pub fn new() -> Self {
        Self::default()
    }
}

/// A completed append range bound to the file generation actually written.
/// The reader remains valid if the journal path is subsequently replaced.
#[derive(Clone)]
pub struct AppendReceipt {
    pub reader: Arc<dyn ReadFile>,
    pub offset: u64,
    pub length: usize,
}

/// An optional, single-owner append capability for a journal.
#[async_trait]
pub trait AppendFile: Send + Sync {
    /// Append and finish the write before acknowledging it. This is the same
    /// visibility boundary as `FileSystem::append_file`, not a power-loss fsync.
    async fn append(&mut self, bytes: &[u8]) -> Result<(), FileError>;

    /// Append with an optional stable-generation range. Missing read permission
    /// or receipt support must not turn a successful append into a failure.
    async fn append_with_receipt(
        &mut self,
        bytes: &[u8],
    ) -> Result<Option<AppendReceipt>, FileError> {
        self.append(bytes).await?;
        Ok(None)
    }
}

/// Read capability bound to one opened file, even if its path is replaced.
/// Implementations serialize cursor access and return errors on short ranges.
#[async_trait]
pub trait ReadFile: Send + Sync {
    async fn read_all(&self) -> Result<Vec<u8>, FileError>;
    async fn read_range(&self, offset: u64, length: usize) -> Result<Vec<u8>, FileError>;

    /// Current byte length of this opened generation, when supported. This is
    /// not a snapshot: later appends or truncation may change the length.
    async fn byte_len(&self) -> Result<Option<u64>, FileError> {
        Ok(None)
    }
}

/// Mirrors TS `FileSystem`. All methods async, never panic.
/// Backends should check cancellation at entry and during long operations.
#[async_trait]
pub trait FileSystem: Send + Sync {
    /// The environment's working directory (absolute).
    fn cwd(&self) -> &Path;

    /// Resolve `path` (relative-to-cwd, `~`, `file://`, absolute) to an absolute
    /// addressed path. Pure — no FS access, no abort. Mirrors TS `absolutePath`.
    async fn absolute_path(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError>;

    /// Join path components. Pure. Mirrors TS `joinPath`.
    async fn join_path(
        &self,
        parts: &[&str],
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError>;

    async fn read_text_file(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<String, FileError>;

    /// Stream-read up to `max_lines` lines (split on `\n`; `max_lines <= 0`
    /// → empty). Mirrors TS `readTextLines{maxLines}`.
    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<String>, FileError>;

    async fn read_binary_file(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<u8>, FileError>;

    /// Creates parent directories (recursive) automatically. Mirrors TS
    /// `writeFile`.
    async fn write_file(
        &self,
        path: &str,
        content: FileContent,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError>;

    async fn append_file(
        &self,
        path: &str,
        content: FileContent,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError>;

    /// Optional reusable append handle for a single-owner journal. Backends
    /// without this capability keep the existing per-call append path.
    async fn open_append_file(
        &self,
        _path: &str,
    ) -> Result<Option<Box<dyn AppendFile>>, FileError> {
        Ok(None)
    }

    /// Optional stable-generation reader. Unsupported backends retain eager
    /// history loading; an actual open failure must be returned, not hidden.
    async fn open_read_file(
        &self,
        _path: &str,
    ) -> Result<Option<std::sync::Arc<dyn ReadFile>>, FileError> {
        Ok(None)
    }

    /// Cut the file at `path` back to `len` bytes, discarding a tail that a
    /// failed append left behind. It never grows a file. Backends without it
    /// fail, so a caller cannot take a tail for clean when it was not cut.
    async fn truncate_file(
        &self,
        path: &str,
        len: u64,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        let _ = (len, cancel);
        Err(FileError::new(
            FileErrorCode::NotSupported,
            "file truncation is unavailable",
        )
        .with_path(path))
    }

    async fn rename_file(
        &self,
        source: &str,
        dest: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError>;

    /// Uses `symlink_metadata` (lstat — symlinks not followed). Mirrors TS
    /// `fileInfo`.
    async fn file_info(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<FileInfo, FileError>;

    async fn list_dir(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<FileInfo>, FileError>;

    /// Search contents with ripgrep. Native backends stream files; virtual
    /// backends feed their bytes to the same matcher and searcher.
    async fn search_contents(
        &self,
        path: &str,
        search: crate::search::ContentSearch,
        cancel: Option<&CancellationToken>,
    ) -> Result<crate::search::ContentSearchResult, FileError> {
        crate::search::search_env(self, path, search, cancel).await
    }

    /// Follows symlinks (`canonicalize`). Mirrors TS `canonicalPath`. Backends
    /// may return `not_found` or `not_supported` for missing/unsupported paths;
    /// callers (mutation queue) fall back to absolute path on those codes.
    async fn canonical_path(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError>;

    /// `false` ONLY for `not_found`; other errors propagate. Mirrors TS
    /// `exists` (which delegates to `fileInfo`, NOT `access(F_OK)`).
    async fn exists(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<bool, FileError>;

    /// Default `recursive=true`. Mirrors TS `createDir{recursive}`.
    async fn create_dir(
        &self,
        path: &str,
        recursive: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError>;

    /// Defaults `recursive=false`, `force=false`. Mirrors TS `remove`.
    async fn remove(
        &self,
        path: &str,
        recursive: bool,
        force: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError>;

    /// Default prefix `"tmp-"`. Mirrors TS `createTempDir{prefix}`.
    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError>;

    /// Mirrors TS `createTempFile{prefix,suffix}` (defaults empty). Note: the
    /// TS impl creates an isolated temp **dir** per file; backends may simplify
    /// to a single file in the OS temp dir (document any divergence).
    async fn create_temp_file(
        &self,
        prefix: &str,
        suffix: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError>;

    /// Best-effort, never returns `Err`. Mirrors TS `cleanup` (kills tracked
    /// child processes, etc.).
    async fn cleanup(&self);
}

/// Mirrors TS `Shell`.
#[async_trait]
pub trait Shell: Send + Sync {
    /// Execute `command` in a bash shell. The TS env resolves a bash binary
    /// (git-bash on Windows, `/bin/bash` on POSIX, `/bin/sh` fallback on POSIX).
    /// There is NO cmd.exe fallback on Windows — failure to find bash is
    /// `ExecutionError::ShellUnavailable`.
    async fn exec<'a>(
        &'a self,
        command: &str,
        options: ShellExecOptions<'a>,
    ) -> Result<ShellOutput, ExecutionError>;

    /// Best-effort, never returns `Err`. Mirrors TS `Shell.cleanup`.
    async fn cleanup(&self);
}

/// Mirrors TS `ExecutionEnv extends FileSystem, Shell`. A marker combining both
/// capabilities — tools receive `Arc<dyn ExecutionEnv>` (or a concrete env) and
/// use `FileSystem` + `Shell` methods interchangeably.
#[async_trait]
pub trait ExecutionEnv: FileSystem + Shell {}

/// Check `cancel` at a method entry point, returning `FileError::aborted()`
/// when set. Mirrors the TS `abortResult<T>(signal, path?)` helper used at the
/// top of every `FileSystem` method.
pub fn check_cancel_file(
    cancel: Option<&CancellationToken>,
    path: Option<&str>,
) -> Result<(), FileError> {
    if let Some(t) = cancel {
        if t.is_cancelled() {
            let mut err = FileError::aborted();
            if let Some(p) = path {
                err = err.with_path(p);
            }
            return Err(err);
        }
    }
    Ok(())
}

/// Same as [`check_cancel_file`] but for `Shell`-side (returns
/// `ExecutionError::aborted()`).
pub fn check_cancel_exec(cancel: Option<&CancellationToken>) -> Result<(), ExecutionError> {
    if let Some(t) = cancel {
        if t.is_cancelled() {
            return Err(ExecutionError::aborted());
        }
    }
    Ok(())
}

#[cfg(test)]
mod capability_tests {
    use super::*;

    // Deliberately implement only the original required methods. These doubles
    // guard source compatibility as well as dispatch through the trait object.
    #[derive(Default)]
    struct LegacyAppender(Vec<u8>);

    #[async_trait]
    impl AppendFile for LegacyAppender {
        async fn append(&mut self, bytes: &[u8]) -> Result<(), FileError> {
            self.0.extend_from_slice(bytes);
            Ok(())
        }
    }

    struct LegacyReader;

    #[async_trait]
    impl ReadFile for LegacyReader {
        async fn read_all(&self) -> Result<Vec<u8>, FileError> {
            Ok(b"legacy".to_vec())
        }

        async fn read_range(&self, _offset: u64, _length: usize) -> Result<Vec<u8>, FileError> {
            Err(FileError::new(FileErrorCode::NotSupported, "legacy reader"))
        }
    }

    #[tokio::test]
    async fn legacy_capabilities_keep_their_default_behavior() {
        let mut writer = LegacyAppender::default();
        let appender: &mut dyn AppendFile = &mut writer;
        assert!(appender
            .append_with_receipt(b"frame\n")
            .await
            .unwrap()
            .is_none());
        assert_eq!(writer.0, b"frame\n");
        let reader: Arc<dyn ReadFile> = Arc::new(LegacyReader);
        assert_eq!(reader.byte_len().await.unwrap(), None);
        assert_eq!(reader.read_all().await.unwrap(), b"legacy");
    }

    #[tokio::test]
    async fn default_receipt_method_preserves_append_errors() {
        struct FailingAppender;
        #[async_trait]
        impl AppendFile for FailingAppender {
            async fn append(&mut self, _bytes: &[u8]) -> Result<(), FileError> {
                Err(FileError::new(FileErrorCode::Unknown, "append failed"))
            }
        }
        assert!(FailingAppender.append_with_receipt(b"frame").await.is_err());
    }
}
