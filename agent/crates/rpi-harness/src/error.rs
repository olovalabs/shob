//! Mirrors `packages/agent/src/harness/session/types.ts::SessionError`.
//!
//! Session-storage failures: the durable layer reports backend failures and
//! protocol violations as `SessionError` with a stable `SessionErrorCode`. The
//! Rust port uses `thiserror` instead of the TS `class extends Error`; `?` /
//! `.map_err` replace `getOrThrow`.

use thiserror::Error;

/// Stable session error codes — mirrors TS `SessionErrorCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionErrorCode {
    NotFound,
    AlreadyExists,
    InvalidEntry,
    InvalidPayload,
    InvalidLane,
    InvalidQuery,
    InvalidForkTarget,
    Storage,
}

impl SessionErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionErrorCode::NotFound => "not_found",
            SessionErrorCode::AlreadyExists => "already_exists",
            SessionErrorCode::InvalidEntry => "invalid_entry",
            SessionErrorCode::InvalidPayload => "invalid_payload",
            SessionErrorCode::InvalidLane => "invalid_lane",
            SessionErrorCode::InvalidQuery => "invalid_query",
            SessionErrorCode::InvalidForkTarget => "invalid_fork_target",
            SessionErrorCode::Storage => "storage",
        }
    }
}

/// Error returned by session storage / state operations. Mirrors TS `SessionError`.
#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct SessionError {
    pub code: SessionErrorCode,
    pub message: String,
}

impl SessionError {
    pub fn new(code: SessionErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(SessionErrorCode::NotFound, message)
    }
    pub fn already_exists(message: impl Into<String>) -> Self {
        Self::new(SessionErrorCode::AlreadyExists, message)
    }
    pub fn invalid_entry(message: impl Into<String>) -> Self {
        Self::new(SessionErrorCode::InvalidEntry, message)
    }
    pub fn invalid_payload(message: impl Into<String>) -> Self {
        Self::new(SessionErrorCode::InvalidPayload, message)
    }
    pub fn invalid_lane(message: impl Into<String>) -> Self {
        Self::new(SessionErrorCode::InvalidLane, message)
    }
    pub fn invalid_query(message: impl Into<String>) -> Self {
        Self::new(SessionErrorCode::InvalidQuery, message)
    }
    pub fn invalid_fork_target(message: impl Into<String>) -> Self {
        Self::new(SessionErrorCode::InvalidForkTarget, message)
    }
    pub fn storage(message: impl Into<String>) -> Self {
        Self::new(SessionErrorCode::Storage, message)
    }
}

pub type SessionResult<T> = std::result::Result<T, SessionError>;
