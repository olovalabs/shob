//! In-memory telemetry context for tests. Collects span starts/ends into a
//! `Vec<(name, SpanStatus)>` so tests can assert which spans ran and how they ended.
//!
//! Mirrors `packages/telemetry/src/testing/conformance.ts` minimal surface.

use crate::context::{SpanGuard, TelemetryContext};
use crate::types::{SpanOptions, SpanStatus};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default, Clone)]
pub struct InMemoryTelemetryContext {
    events: Arc<Mutex<Vec<SpanEvent>>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpanEvent {
    pub name: String,
    pub status: SpanStatus,
}

impl InMemoryTelemetryContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> Vec<SpanEvent> {
        self.events.lock().expect("events lock poisoned").clone()
    }

    pub fn clear(&self) {
        self.events.lock().expect("events lock poisoned").clear();
    }
}

impl TelemetryContext for InMemoryTelemetryContext {
    fn start_span(&self, options: SpanOptions) -> SpanGuard {
        let name = options.name.clone();
        let events = Arc::clone(&self.events);
        SpanGuard::new(name.clone(), move |_n, status| {
            events
                .lock()
                .expect("events lock poisoned")
                .push(SpanEvent {
                    name: name.clone(),
                    status,
                });
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_and_inmemory_compile_and_run() {
        let noop = crate::NoopTelemetryContext;
        let _g = noop.start_span(SpanOptions::new("noop-span"));
        drop(_g);

        let ctx = InMemoryTelemetryContext::new();
        let g = ctx.start_span(SpanOptions::new("inmem-span"));
        drop(g);
        assert_eq!(
            ctx.events(),
            vec![SpanEvent {
                name: "inmem-span".into(),
                status: SpanStatus::Ok,
            }]
        );
    }
}
