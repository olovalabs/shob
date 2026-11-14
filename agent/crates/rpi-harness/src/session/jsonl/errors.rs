//! Mirrors `packages/agent/src/harness/session/jsonl/errors.ts` — JSONL
//! decode errors + the `fileResult` / `invalidFile` adapters that map file
//! errors into [`SessionError`].
//!
//! `JsonlDecodeError` keeps the TS two-kind taxonomy (`syntax` vs `schema`)
//! because torn-tail recovery keys off `kind === "syntax"` (only a syntactically
//! malformed *last* line is a recoverable torn tail).

use rpi_tools::error::{FileError, FileErrorCode};

use crate::error::{SessionError, SessionErrorCode};

/// `syntax` | `schema`. The kind drives torn-tail recovery: only a *syntax*
/// error on the *last* physical line is treated as an unacknowledged partial
/// append and repaired; anything else is hard corruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonlDecodeErrorKind {
    Syntax,
    Schema,
}

/// A JSONL line failed to parse. Mirrors TS `JsonlDecodeError`.
///
/// The TS class attaches the underlying `Error` as `cause`; the Rust port folds
/// the cause into `message` (the `kind` is what recovery keys on, and the
/// message is surfaced verbatim by [`invalid_file`]).
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct JsonlDecodeError {
    pub kind: JsonlDecodeErrorKind,
    pub message: String,
}

impl JsonlDecodeError {
    pub fn syntax(message: impl Into<String>) -> Self {
        Self {
            kind: JsonlDecodeErrorKind::Syntax,
            message: message.into(),
        }
    }
    pub fn schema(message: impl Into<String>) -> Self {
        Self {
            kind: JsonlDecodeErrorKind::Schema,
            message: message.into(),
        }
    }
}

/// Mirrors TS `fileResult`: unwrap a `Result<T, FileError>`, mapping `not_found`
/// to [`SessionErrorCode::NotFound`] and everything else to
/// [`SessionErrorCode::Storage`], prefixing `message`.
///
/// The TS variant preserves the original `FileError` as `cause`; `SessionError`
/// is `Clone` and carries no boxed source, so the cause text is folded into the
/// message (the code + message are what callers match on).
pub fn file_result<T>(result: Result<T, FileError>, message: &str) -> Result<T, SessionError> {
    result.map_err(|e| {
        let code = if e.code == FileErrorCode::NotFound {
            SessionErrorCode::NotFound
        } else {
            SessionErrorCode::Storage
        };
        SessionError::new(code, format!("{message}: {}", e.message))
    })
}

/// Mirrors TS `invalidFile`: build an `invalid_entry` `SessionError` pointing
/// at the offending line. `cause` is rendered via its `Display`.
pub fn invalid_file<T: std::fmt::Display>(path: &str, line: u32, cause: &T) -> SessionError {
    SessionError::invalid_entry(format!(
        "Invalid JSONL v4 session {path}: line {line} {cause}"
    ))
}
