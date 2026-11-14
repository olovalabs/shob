//! Mirrors `packages/agent/src/harness/types.ts` — the `FileError` /
//! `ExecutionError` shapes + their stable code enums used by the `ExecutionEnv`
//! trait. Drops the TS `ok/err/getOrThrow` helpers (Rust has these natively) and
//! the `CompactionError`/`BranchSummaryError` (those live in `pi-harness`).
//!
//! The stable code enums are backend-independent: `OsExecutionEnv` and
//! `InMemoryExecutionEnv` both classify failures into the same codes so tool
//! logic and tests can match on them without knowing the backend.

use std::fmt;

/// Backend-independent file-error codes. Mirrors TS `FileErrorCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileErrorCode {
    Aborted,
    NotFound,
    PermissionDenied,
    NotDirectory,
    IsDirectory,
    Invalid,
    NotSupported,
    Unknown,
}

impl FileErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            FileErrorCode::Aborted => "aborted",
            FileErrorCode::NotFound => "not_found",
            FileErrorCode::PermissionDenied => "permission_denied",
            FileErrorCode::NotDirectory => "not_directory",
            FileErrorCode::IsDirectory => "is_directory",
            FileErrorCode::Invalid => "invalid",
            FileErrorCode::NotSupported => "not_supported",
            FileErrorCode::Unknown => "unknown",
        }
    }
}

impl fmt::Display for FileErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned by `FileSystem` operations. Mirrors TS `FileError`.
///
/// `path` is the absolute addressed path associated with the failure, when
/// available. `source` carries the underlying backend error (e.g. an
/// `io::Error`) for diagnostics; it is `Box<dyn Error + Send + Sync>` so both
/// `OsExecutionEnv` (`io::Error`) and `InMemoryExecutionEnv` (no source) fit.
#[derive(Debug)]
pub struct FileError {
    pub code: FileErrorCode,
    pub message: String,
    pub path: Option<String>,
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl FileError {
    pub fn new(code: FileErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            path: None,
            source: None,
        }
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn with_source(mut self, source: Box<dyn std::error::Error + Send + Sync>) -> Self {
        self.source = Some(source);
        self
    }

    /// Convenience for the common `NotFound` at a path.
    pub fn not_found(path: impl Into<String>) -> Self {
        let p: String = path.into();
        Self::new(FileErrorCode::NotFound, format!("path not found: {p}")).with_path(p)
    }

    /// Convenience for an abort (cancellation).
    pub fn aborted() -> Self {
        Self::new(FileErrorCode::Aborted, "operation aborted")
    }
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(p) => write!(f, "{}: {} (path: {})", self.code, self.message, p),
            None => write!(f, "{}: {}", self.code, self.message),
        }
    }
}

impl std::error::Error for FileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|b| b.as_ref() as &(dyn std::error::Error + 'static))
    }
}

/// Backend-independent execution-error codes. Mirrors TS `ExecutionErrorCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionErrorCode {
    Aborted,
    Timeout,
    ShellUnavailable,
    SpawnError,
    CallbackError,
    Unknown,
}

impl ExecutionErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ExecutionErrorCode::Aborted => "aborted",
            ExecutionErrorCode::Timeout => "timeout",
            ExecutionErrorCode::ShellUnavailable => "shell_unavailable",
            ExecutionErrorCode::SpawnError => "spawn_error",
            ExecutionErrorCode::CallbackError => "callback_error",
            ExecutionErrorCode::Unknown => "unknown",
        }
    }
}

impl fmt::Display for ExecutionErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned by `Shell::exec`. Mirrors TS `ExecutionError`.
///
/// `source` is stored as a boxed `String`-ified error (not a `dyn Error`) so
/// `ExecutionError` is `Clone` — the TS `cause` is only reflected into the
/// `message` for diagnostic purposes, and cloning lets the bash capture layer
/// carry an `execution_error` through a `Clone` result unmodified.
#[derive(Debug, Clone)]
pub struct ExecutionError {
    pub code: ExecutionErrorCode,
    pub message: String,
    pub source_message: Option<String>,
}

impl ExecutionError {
    pub fn new(code: ExecutionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            source_message: None,
        }
    }

    pub fn with_source(mut self, source: Box<dyn std::error::Error + Send + Sync>) -> Self {
        self.source_message = Some(source.to_string());
        self
    }

    pub fn with_source_message(mut self, source: impl Into<String>) -> Self {
        self.source_message = Some(source.into());
        self
    }

    pub fn aborted() -> Self {
        Self::new(ExecutionErrorCode::Aborted, "command aborted")
    }

    pub fn timeout(seconds: u64) -> Self {
        Self::new(
            ExecutionErrorCode::Timeout,
            format!("command timed out after {seconds}s"),
        )
    }
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ExecutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // `source_message` is a plain String, not a boxed error, so there is no
        // chainable `source`. The message already incorporates the cause text.
        None
    }
}

/// Classify an `io::Error` (from `tokio::fs` / std) into a `FileError` code.
/// Mirrors the mapping the TS Node.js env applies when it catches `io::Error`.
pub fn io_to_file_code(err: &std::io::Error) -> FileErrorCode {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::NotFound => FileErrorCode::NotFound,
        ErrorKind::PermissionDenied => FileErrorCode::PermissionDenied,
        ErrorKind::AlreadyExists => FileErrorCode::Invalid,
        ErrorKind::TimedOut => FileErrorCode::Aborted,
        ErrorKind::Unsupported => FileErrorCode::NotSupported,
        ErrorKind::Interrupted => FileErrorCode::Aborted,
        _ => match err.raw_os_error() {
            // POSIX EISDIR / ENOTDIR. These ErrorKind variants stabilized
            // after the workspace MSRV, so retain the classification through
            // their portable Unix errno values.
            #[cfg(unix)]
            Some(21) => FileErrorCode::IsDirectory,
            #[cfg(unix)]
            Some(20) => FileErrorCode::NotDirectory,
            // Windows ERROR_DIRECTORY.
            #[cfg(windows)]
            Some(267) => FileErrorCode::NotDirectory,
            _ => FileErrorCode::Unknown,
        },
    }
}

/// Build a `FileError` from an `io::Error`, classifying the code + preserving
/// the source. `path` is attached when the caller knows it.
pub fn io_to_file_error(err: std::io::Error, path: Option<&str>) -> FileError {
    let code = io_to_file_code(&err);
    let message = err.to_string();
    let mut e = FileError::new(code, message).with_source(Box::new(err));
    if let Some(p) = path {
        e = e.with_path(p);
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_error_codes_round_trip() {
        for code in [
            FileErrorCode::Aborted,
            FileErrorCode::NotFound,
            FileErrorCode::PermissionDenied,
            FileErrorCode::NotDirectory,
            FileErrorCode::IsDirectory,
            FileErrorCode::Invalid,
            FileErrorCode::NotSupported,
            FileErrorCode::Unknown,
        ] {
            assert_eq!(FileErrorCode::as_str(code), code.to_string());
        }
    }

    #[test]
    fn execution_error_codes_round_trip() {
        for code in [
            ExecutionErrorCode::Aborted,
            ExecutionErrorCode::Timeout,
            ExecutionErrorCode::ShellUnavailable,
            ExecutionErrorCode::SpawnError,
            ExecutionErrorCode::CallbackError,
            ExecutionErrorCode::Unknown,
        ] {
            assert_eq!(ExecutionErrorCode::as_str(code), code.to_string());
        }
    }

    #[test]
    fn io_error_classifies_not_found() {
        let err = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(io_to_file_code(&err), FileErrorCode::NotFound);
    }

    #[cfg(windows)]
    #[test]
    fn io_error_classifies_windows_not_directory() {
        let err = std::io::Error::from_raw_os_error(267);
        assert_eq!(io_to_file_code(&err), FileErrorCode::NotDirectory);
    }

    #[cfg(unix)]
    #[test]
    fn io_error_classifies_posix_directory_kinds() {
        assert_eq!(
            io_to_file_code(&std::io::Error::from_raw_os_error(21)),
            FileErrorCode::IsDirectory
        );
        assert_eq!(
            io_to_file_code(&std::io::Error::from_raw_os_error(20)),
            FileErrorCode::NotDirectory
        );
    }

    #[test]
    fn io_to_file_error_preserves_path_and_source() {
        let err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let fe = io_to_file_error(err, Some("/x/y"));
        assert_eq!(fe.code, FileErrorCode::PermissionDenied);
        assert_eq!(fe.path.as_deref(), Some("/x/y"));
        assert!(fe.source.is_some());
    }

    #[test]
    fn display_formats_path_when_present() {
        let s = FileError::not_found("/a/b").to_string();
        assert!(s.contains("not_found"));
        assert!(s.contains("/a/b"));
        let s2 = FileError::new(FileErrorCode::Invalid, "bad").to_string();
        assert!(s2.contains("invalid: bad"));
        assert!(!s2.contains("path"));
    }
}
