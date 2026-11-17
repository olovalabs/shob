//! Mirrors `packages/telemetry/src`. Minimal span/event contracts with a noop
//! default implementation.
//!
//! Real providers and the agent loop call into [`TelemetryContext`] at request
//! boundaries. The default [`NoopTelemetryContext`] does nothing. Consumers stay
//! monomorphic by holding a `&dyn TelemetryContext` defaulting to
//! [`NOOP_TELEMETRY_CONTEXT`].
//!
//! Telemetry in the TS source is a thin span/attribute layer used opportunistically;
//! we port only the surface downstream crates actually touch.

#![forbid(unsafe_code)]

pub mod context;
pub mod noop;
pub mod production;
pub mod types;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use context::{SpanGuard, TelemetryContext};
pub use noop::{NoopTelemetryContext, NOOP_TELEMETRY_CONTEXT};
pub use production::ProductionTelemetryContext;
pub use types::{AttributeValue, SpanAttributes, SpanOptions, SpanStatus};

/// Error returned by span operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryError {
    /// The operation running under the span was cancelled (abort/timeout).
    Cancelled,
}

impl std::fmt::Display for TelemetryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TelemetryError::Cancelled => write!(f, "operation cancelled"),
        }
    }
}

impl std::error::Error for TelemetryError {}
