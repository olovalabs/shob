//! Mirrors the error surfaces implied by `packages/ai/src` — no single TS file;
//! the variants collect every failure mode the ai crate can surface to a caller.
//!
//! Providers convert their wire-level failures into `AiError` before encoding
//! them as an `AssistantMessageEvent::Error` (so the stream never panics). The
//! agent loop and harness see `AiError` only when they themselves drive a
//! provider call directly (e.g. compaction's standalone stream).

use std::fmt;

/// Every failure mode the `pi-ai` crate can surface.
#[derive(Debug, Clone, PartialEq)]
pub enum AiError {
    /// HTTP transport failure (connection, DNS, TLS, non-retryable status).
    Http {
        status: Option<u16>,
        message: String,
    },
    /// SSE parse / stream-corruption failure.
    Sse { message: String },
    /// Auth failure (401/403, missing/expired key).
    Auth { message: String },
    /// Operation cancelled via `CancellationToken`.
    Abort { message: String },
    /// JSON-Schema validation of tool arguments failed (after coercion).
    Schema { message: String },
    /// Provider returned a structured error body.
    Provider { code: String, message: String },
    /// Usage / cost computation failed (malformed provider usage payload).
    Usage { message: String },
}

impl fmt::Display for AiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AiError::Http {
                status: Some(s),
                message,
            } => {
                write!(f, "http error {s}: {message}")
            }
            AiError::Http {
                status: None,
                message,
            } => write!(f, "http error: {message}"),
            AiError::Sse { message } => write!(f, "sse error: {message}"),
            AiError::Auth { message } => write!(f, "auth error: {message}"),
            AiError::Abort { message } => write!(f, "abort error: {message}"),
            AiError::Schema { message } => write!(f, "schema error: {message}"),
            AiError::Provider { code, message } => write!(f, "provider error [{code}]: {message}"),
            AiError::Usage { message } => write!(f, "usage error: {message}"),
        }
    }
}

impl std::error::Error for AiError {}

impl AiError {
    /// True if this error originated from cancellation (not a provider fault).
    pub fn is_abort(&self) -> bool {
        matches!(self, AiError::Abort { .. })
    }

    /// True if retryable under `retry_provider_request` — matches the TS retry
    /// predicate (408/409/429/>=500 plus transport-level Http with no status).
    pub fn is_retryable(&self) -> bool {
        match self {
            AiError::Http {
                status: Some(s), ..
            } => matches!(*s, 408 | 409 | 429) || *s >= 500,
            AiError::Http { status: None, .. } => true,
            _ => false,
        }
    }
}
