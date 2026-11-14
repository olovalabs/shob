//! Telemetry context trait + RAII span guard.
//!
//! Mirrors the TS `TelemetryContext` interface. The contract is intentionally
//! minimal: callers start a named span, run a body, and the guard records the
//! terminal status on drop or via explicit [`SpanGuard::finish`].

use crate::types::{SpanOptions, SpanStatus};
use std::sync::Arc;

/// A telemetry backend. Implementations: [`crate::NoopTelemetryContext`] (default),
/// in-memory test context (`#[cfg(test)]`).
///
/// All methods are synchronous and infallible at the API boundary — backend
/// failures are swallowed by the implementation, matching the TS "telemetry must
/// never break the call" contract.
pub trait TelemetryContext: Send + Sync {
    /// Start a span with the given options. Returns an RAII guard that records the
    /// span end (status = `Ok` unless overridden) when dropped.
    fn start_span(&self, options: SpanOptions) -> SpanGuard;
}

/// RAII handle for a started span. On drop, records the span end with the current
/// status (default [`SpanStatus::Ok`]).
///
/// Cloning a guard creates a second handle to the *same* span end callback; the
/// span ends when the **last** clone drops.
pub struct SpanGuard {
    inner: Option<Arc<SpanEnd>>,
}

struct SpanEnd {
    name: String,
    finish: Mutex<Option<SpanStatus>>,
    on_end: Box<dyn FnOnce(&str, SpanStatus) + Send + Sync>,
}

use std::sync::Mutex;

impl SpanGuard {
    /// Create a no-op guard that does nothing on drop (used by the noop context and
    /// for spans the caller doesn't care to track).
    pub fn noop() -> Self {
        Self { inner: None }
    }

    /// Create a guard with a custom end callback.
    pub fn new<F>(name: String, on_end: F) -> Self
    where
        F: FnOnce(&str, SpanStatus) + Send + Sync + 'static,
    {
        Self {
            inner: Some(Arc::new(SpanEnd {
                name,
                finish: Mutex::new(None),
                on_end: Box::new(on_end),
            })),
        }
    }

    /// Explicitly set the terminal status. If not called, `Ok` is recorded on drop.
    pub fn finish(&self, status: SpanStatus) {
        if let Some(inner) = &self.inner {
            if let Ok(mut slot) = inner.finish.lock() {
                *slot = Some(status);
            }
        }
    }

    /// Span name (empty string for the noop guard).
    pub fn name(&self) -> &str {
        self.inner.as_ref().map(|e| e.name.as_str()).unwrap_or("")
    }
}

impl Clone for SpanGuard {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        // Another clone is still alive: defer the end callback to the last drop.
        if Arc::strong_count(&inner) > 1 {
            return;
        }
        // Last handle: unwrap the Arc and fire the end callback exactly once.
        let Ok(owned) = Arc::try_unwrap(inner) else {
            return;
        };
        let status = owned
            .finish
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .unwrap_or(SpanStatus::Ok);
        (owned.on_end)(&owned.name, status);
    }
}
