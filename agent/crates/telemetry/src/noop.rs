//! Noop telemetry context — the default backend. Does nothing and allocates nothing
//! beyond a `SpanGuard::noop()`.

use crate::context::{SpanGuard, TelemetryContext};
use crate::types::SpanOptions;

/// A telemetry context that records nothing. `Clone`/`Copy` so it can be stored by
/// value and shared cheaply.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopTelemetryContext;

impl TelemetryContext for NoopTelemetryContext {
    fn start_span(&self, _options: SpanOptions) -> SpanGuard {
        SpanGuard::noop()
    }
}

/// A static noop context consumers can default to without any allocation.
pub static NOOP_TELEMETRY_CONTEXT: NoopTelemetryContext = NoopTelemetryContext;
