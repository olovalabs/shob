//! Optional, protocol-neutral text I/O for the existing native tools.
//!
//! This is bounded text delegation, NOT an ExecutionEnv replacement or an OS
//! sandbox. The caller must configure only while its session is idle, and own
//! backend shutdown/timeouts. Each tool freezes configuration before resolving
//! its path. Backends must not retain the owning harness/session (use weak
//! references where necessary). No capabilities are enabled by default.
//!
//! Mutations serialize by router + lexical path, not symlink identity. There is
//! no editor version/CAS, rollback, or retry after an unconfirmed write: a lost
//! acknowledgment may mean the client already committed the change.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use futures::future::BoxFuture;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::error::{FileError, FileErrorCode};
use crate::path_utils::normalize_tool_path;

pub const MAX_FILE_IO_BYTES: usize = 256 * 1024;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FileIOCapabilities {
    pub read_text: bool,
    pub write_text: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileIOReadRange {
    /// One-based offset, applied by the backend exactly once. Content is raw
    /// text, not numbered/display-truncated. Empty content at EOF is valid.
    Lines { offset_1based: u32, limit: u32 },
    /// Full content for edit. A backend must fail rather than truncate it.
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileIORequest {
    ReadText {
        absolute_path: String,
        range: FileIOReadRange,
        max_bytes: usize,
    },
    WriteText {
        absolute_path: String,
        content: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileIOResponse {
    Text { content: String },
    Written,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileIOErrorCode {
    Unsupported,
    Denied,
    NotFound,
    InvalidResponse,
    TooLarge,
    Timeout,
    Cancelled,
    Disconnected,
    InvalidPath,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct FileIOError {
    pub code: FileIOErrorCode,
    pub message: String,
}

impl FileIOError {
    pub fn new(code: FileIOErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

// Keep the existing tools' FileError -> AgentError surface. Preserve the typed
// client error as source instead of adding variants to unrelated native traits.
impl From<FileIOError> for FileError {
    fn from(error: FileIOError) -> Self {
        let code = match error.code {
            FileIOErrorCode::Unsupported => FileErrorCode::NotSupported,
            FileIOErrorCode::Denied => FileErrorCode::PermissionDenied,
            FileIOErrorCode::NotFound => FileErrorCode::NotFound,
            FileIOErrorCode::Cancelled => FileErrorCode::Aborted,
            FileIOErrorCode::InvalidPath
            | FileIOErrorCode::InvalidResponse
            | FileIOErrorCode::TooLarge => FileErrorCode::Invalid,
            FileIOErrorCode::Timeout | FileIOErrorCode::Disconnected => FileErrorCode::Unknown,
        };
        FileError::new(code, error.to_string()).with_source(Box::new(error))
    }
}

#[async_trait]
pub trait FileIOBackend: Send + Sync {
    /// The transport owner supplies deadlines, bounded admission/ingress, and
    /// retirement on future drop. Never retry writes automatically. Checking
    /// max_bytes after decoding does not bound transport allocation by itself.
    async fn request(
        &self,
        request: FileIORequest,
        cancel: CancellationToken,
    ) -> Result<FileIOResponse, FileIOError>;
}

#[derive(Debug, Clone, Copy)]
pub enum FileIOOperation {
    Read,
    Write,
    Edit,
}

#[derive(Default, Clone)]
struct Configuration {
    capabilities: FileIOCapabilities,
    backend: Option<Arc<dyn FileIOBackend>>,
}

#[derive(Default)]
pub struct FileIORouter {
    configuration: Mutex<Configuration>,
    queues: Mutex<HashMap<String, Weak<Mutex<()>>>>,
}

impl FileIORouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomic configuration replacement; the CLI owner MUST ensure idle state.
    /// This layer has no run/harness knowledge. Existing operation snapshots
    /// retain their original capability/backend choice across queue waits.
    pub async fn configure(
        &self,
        capabilities: FileIOCapabilities,
        backend: Option<Arc<dyn FileIOBackend>>,
    ) -> Result<(), FileIOError> {
        if (capabilities.read_text || capabilities.write_text) && backend.is_none() {
            return Err(FileIOError::new(
                FileIOErrorCode::Unsupported,
                "selected client I/O requires a backend",
            ));
        }
        *self.configuration.lock().await = Configuration {
            capabilities,
            backend,
        };
        Ok(())
    }

    /// None means the original local implementation. Errors NEVER mean local.
    pub async fn select(
        &self,
        operation: FileIOOperation,
    ) -> Result<Option<FileIOSnapshot>, FileIOError> {
        let config = self.configuration.lock().await.clone();
        let caps = config.capabilities;
        let client = match operation {
            FileIOOperation::Read => caps.read_text,
            FileIOOperation::Write if caps.read_text && !caps.write_text => {
                return Err(FileIOError::new(
                    FileIOErrorCode::Unsupported,
                    "write requires client write_text when client read_text is selected",
                ));
            }
            FileIOOperation::Write => caps.write_text,
            FileIOOperation::Edit if caps.read_text != caps.write_text => {
                return Err(FileIOError::new(
                    FileIOErrorCode::Unsupported,
                    "edit requires both client read_text and write_text",
                ));
            }
            FileIOOperation::Edit => caps.read_text && caps.write_text,
        };
        if !client {
            return Ok(None);
        }
        let backend = config.backend.ok_or_else(|| {
            FileIOError::new(FileIOErrorCode::Unsupported, "client backend unavailable")
        })?;
        Ok(Some(FileIOSnapshot {
            backend,
            capabilities: caps,
        }))
    }

    /// Shared by client write AND edit. Cancellation never skips a queue turn.
    pub(crate) async fn with_mutation_queue<F, R>(
        &self,
        absolute_path: &str,
        cancel: &CancellationToken,
        f: F,
    ) -> Result<R, FileError>
    where
        F: for<'a> FnOnce(&'a CancellationToken) -> BoxFuture<'a, Result<R, FileError>>,
        R: Send + 'static,
    {
        let mutex = {
            let mut queues = self.queues.lock().await;
            queues.retain(|_, value| value.strong_count() > 0);
            if let Some(existing) = queues.get(absolute_path).and_then(Weak::upgrade) {
                existing
            } else {
                let mutex = Arc::new(Mutex::new(()));
                queues.insert(absolute_path.to_owned(), Arc::downgrade(&mutex));
                mutex
            }
        };
        let _guard = mutex.lock().await;
        f(cancel).await
    }
}

/// Select only the mutation queue; local filesystem semantics stay in the tools.
pub(crate) async fn with_tool_mutation_queue<F, R>(
    router: Option<&FileIORouter>,
    env: &dyn crate::file_mutation_queue::MutatingEnv,
    path: &str,
    cancel: &CancellationToken,
    f: F,
) -> Result<R, FileError>
where
    F: for<'a> FnOnce(&'a CancellationToken) -> BoxFuture<'a, Result<R, FileError>>,
    R: Send + 'static,
{
    match router {
        Some(router) => router.with_mutation_queue(path, cancel, f).await,
        None => crate::file_mutation_queue::with_file_mutation_queue(env, path, cancel, f).await,
    }
}

/// Immutable authority/backend choice for one complete tool operation.
#[derive(Clone)]
pub struct FileIOSnapshot {
    backend: Arc<dyn FileIOBackend>,
    capabilities: FileIOCapabilities,
}

impl FileIOSnapshot {
    pub async fn read_text(
        &self,
        path: &str,
        range: FileIOReadRange,
        cancel: &CancellationToken,
    ) -> Result<String, FileIOError> {
        if !self.capabilities.read_text {
            return Err(FileIOError::new(
                FileIOErrorCode::Unsupported,
                "client text read not selected",
            ));
        }
        check_cancel(cancel)?;
        let response = self
            .backend
            .request(
                FileIORequest::ReadText {
                    absolute_path: path.to_owned(),
                    range,
                    max_bytes: MAX_FILE_IO_BYTES,
                },
                cancel.clone(),
            )
            .await;
        check_cancel(cancel)?;
        match response? {
            FileIOResponse::Text { content } => {
                check_text_size(&content)?;
                if let FileIOReadRange::Lines { limit, .. } = range {
                    // A trailing newline is a terminator, not a sentinel line.
                    if crate::truncate::split_lines_for_counting(&content).len() > limit as usize {
                        return Err(FileIOError::new(
                            FileIOErrorCode::InvalidResponse,
                            "client returned more lines than requested",
                        ));
                    }
                }
                Ok(content)
            }
            _ => Err(FileIOError::new(
                FileIOErrorCode::InvalidResponse,
                "expected client text response",
            )),
        }
    }

    pub async fn write_text(
        &self,
        path: &str,
        content: String,
        cancel: &CancellationToken,
    ) -> Result<(), FileIOError> {
        if !self.capabilities.write_text {
            return Err(FileIOError::new(
                FileIOErrorCode::Unsupported,
                "client text write not selected",
            ));
        }
        check_cancel(cancel)?;
        check_text_size(&content)?;
        let response = self
            .backend
            .request(
                FileIORequest::WriteText {
                    absolute_path: path.to_owned(),
                    content,
                },
                cancel.clone(),
            )
            .await;
        check_cancel(cancel)?;
        match response? {
            FileIOResponse::Written => Ok(()),
            _ => Err(FileIOError::new(
                FileIOErrorCode::InvalidResponse,
                "expected client write acknowledgment",
            )),
        }
    }
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), FileIOError> {
    if cancel.is_cancelled() {
        return Err(FileIOError::new(
            FileIOErrorCode::Cancelled,
            "operation cancelled; a dispatched write may already have committed",
        ));
    }
    Ok(())
}

fn check_text_size(content: &str) -> Result<(), FileIOError> {
    if content.len() > MAX_FILE_IO_BYTES {
        return Err(FileIOError::new(
            FileIOErrorCode::TooLarge,
            "client text exceeds 256 KiB UTF-8 limit",
        ));
    }
    Ok(())
}

/// Pure addressing: never call env.absolute_path, exists, stat or canonicalize.
/// Supports explicit POSIX, drive-absolute and UNC namespaces on any host;
/// preserves case. Relative paths use cwd's namespace. Home/file URI expansion,
/// device namespaces and ambiguous Windows drive/root-relative paths are not
/// supported (they would require host-specific authority).
pub fn resolve_client_path(cwd: &Path, path: &str) -> Result<String, FileIOError> {
    fn invalid() -> FileIOError {
        FileIOError::new(
            FileIOErrorCode::InvalidPath,
            "unsupported or ambiguous client path",
        )
    }
    fn absolute(value: &str) -> Result<Option<(String, String, bool)>, FileIOError> {
        if value.starts_with("\\\\?\\")
            || value.starts_with("\\\\.\\")
            || value.starts_with("//?/")
            || value.starts_with("//./")
        {
            return Err(invalid());
        }
        if value.starts_with("\\\\") || value.starts_with("//") {
            let normalized = value.replace('\\', "/");
            let mut parts = normalized[2..].split('/');
            let server = parts.next().unwrap_or("");
            let share = parts.next().unwrap_or("");
            if server.is_empty()
                || share.is_empty()
                || [server, share]
                    .iter()
                    .any(|v| *v == "." || *v == ".." || v.contains(':'))
            {
                return Err(invalid());
            }
            return Ok(Some((
                format!("//{server}/{share}/"),
                parts.collect::<Vec<_>>().join("/"),
                true,
            )));
        }
        if value.as_bytes().get(1) == Some(&b':') {
            if !value.as_bytes()[0].is_ascii_alphabetic()
                || !matches!(value.as_bytes().get(2), Some(b'/') | Some(b'\\'))
            {
                return Err(invalid());
            }
            return Ok(Some((
                format!("{}/", &value[..2]),
                value[3..].replace('\\', "/"),
                true,
            )));
        }
        if value.starts_with('/') {
            return Ok(Some(("/".into(), value[1..].into(), false)));
        }
        if value.starts_with('\\') {
            return Err(invalid());
        }
        Ok(None)
    }
    let path = normalize_tool_path(path);
    if path.is_empty() || path.contains('\0') || path.starts_with('~') || path.contains("://") {
        return Err(invalid());
    }
    let (root, tail, windows) = if let Some(abs) = absolute(&path)? {
        abs
    } else {
        let cwd = cwd.to_str().ok_or_else(invalid)?;
        let (root, base, windows) = absolute(cwd)?.ok_or_else(invalid)?;
        (root, format!("{base}/{path}"), windows)
    };
    if tail.contains(':') || (!windows && tail.contains('\\')) {
        return Err(invalid());
    }
    let tail = if windows {
        tail.replace('\\', "/")
    } else {
        tail
    };
    let mut components = Vec::new();
    for part in tail.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            _ => components.push(part),
        }
    }
    Ok(format!("{root}{}", components.join("/")))
}

#[cfg(test)]
#[path = "file_io_tests.rs"]
mod tests;
