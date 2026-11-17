//! Opt-in startup spans. Only fixed phase names and numeric values reach stderr.
//! Spans nest; their durations must not be added as independent measurements.
use std::{
    ffi::OsStr,
    io::Write,
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    },
    time::Instant,
};

#[derive(Clone, Copy, Debug)]
pub(crate) enum Phase {
    StartupToHarness,
    CwdLookup,
    ExtensionPrepare,
    ExtensionRegister,
    BindingFacts,
    ModelResolve,
    HarnessBuild,
    WriterRestore,
    MetadataLookup,
    ResourceObservation,
    InventoryBind,
    BeforeStartHooks,
    SessionStartHooks,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::StartupToHarness => "startup_to_harness",
            Self::CwdLookup => "cwd_lookup",
            Self::ExtensionPrepare => "extension_prepare",
            Self::ExtensionRegister => "extension_register",
            Self::BindingFacts => "binding_facts",
            Self::ModelResolve => "model_resolve",
            Self::HarnessBuild => "harness_build",
            Self::WriterRestore => "writer_restore",
            Self::MetadataLookup => "metadata_lookup",
            Self::ResourceObservation => "resource_observation",
            Self::InventoryBind => "inventory_bind",
            Self::BeforeStartHooks => "before_start_hooks",
            Self::SessionStartHooks => "session_start_hooks",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Begin,
    End,
    Incomplete,
}

impl Event {
    fn name(self) -> &'static str {
        match self {
            Self::Begin => "begin",
            Self::End => "end",
            Self::Incomplete => "incomplete",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Record {
    phase: Phase,
    span: u64,
    event: Event,
    at_ns: u64,
    elapsed_ns: u64,
}

impl std::fmt::Display for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RPI_STARTUP_DIAGNOSTICS pid={} span={} phase={} event={} at_ns={} elapsed_ns={}",
            std::process::id(),
            self.span,
            self.phase.name(),
            self.event.name(),
            self.at_ns,
            self.elapsed_ns,
        )
    }
}

fn emit(record: Record) {
    // Diagnostics must not panic or change startup results if stderr is closed.
    let _ = writeln!(std::io::stderr().lock(), "{record}");
}

fn enabled(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("1"))
}

fn nanos(duration: std::time::Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}

static ORIGIN: OnceLock<Instant> = OnceLock::new();
static NEXT_SPAN: AtomicU64 = AtomicU64::new(1);

struct Measurement {
    phase: Phase,
    span: u64,
    started: Instant,
    sink: fn(Record),
}

impl Measurement {
    fn record(&self, event: Event) {
        let origin = *ORIGIN.get_or_init(Instant::now);
        let now = Instant::now();
        (self.sink)(Record {
            phase: self.phase,
            span: self.span,
            event,
            at_ns: nanos(now.duration_since(origin)),
            elapsed_ns: nanos(now.duration_since(self.started)),
        });
    }
}

/// `end` means this measured operation returned normally, not HTTP/business
/// success. An early return, error propagated with `?`, panic or cancellation
/// drops an unfinished span as `incomplete`. No errors or arguments are logged.
pub(crate) struct StartupSpan(Option<Measurement>);

impl StartupSpan {
    pub(crate) fn start(phase: Phase) -> Self {
        Self::with_sink(
            phase,
            enabled(std::env::var_os("RPI_STARTUP_DIAGNOSTICS").as_deref()),
            emit,
        )
    }

    fn with_sink(phase: Phase, enabled: bool, sink: fn(Record)) -> Self {
        let measurement = enabled.then(|| {
            ORIGIN.get_or_init(Instant::now);
            let measurement = Measurement {
                phase,
                span: NEXT_SPAN.fetch_add(1, Ordering::Relaxed),
                started: Instant::now(),
                sink,
            };
            measurement.record(Event::Begin);
            measurement
        });
        Self(measurement)
    }

    pub(crate) fn finish(mut self) {
        if let Some(measurement) = self.0.take() {
            measurement.record(Event::End);
        }
    }
}

impl Drop for StartupSpan {
    fn drop(&mut self) {
        if let Some(measurement) = self.0.take() {
            measurement.record(Event::Incomplete);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        static RECORDS: RefCell<Vec<Record>> = const { RefCell::new(Vec::new()) };
    }

    fn collect(record: Record) {
        RECORDS.with_borrow_mut(|records| records.push(record));
    }

    fn records() -> Vec<Record> {
        RECORDS.with_borrow_mut(std::mem::take)
    }

    #[test]
    fn switch_requires_exact_one_without_mutating_environment() {
        assert!(enabled(Some(OsStr::new("1"))));
        assert!(!enabled(None));
        for value in ["", "0", "true", "yes", " 1", "1 "] {
            assert!(!enabled(Some(OsStr::new(value))));
        }
    }

    #[test]
    fn disabled_spans_emit_nothing_on_finish_or_drop() {
        records();
        StartupSpan::with_sink(Phase::BindingFacts, false, collect).finish();
        drop(StartupSpan::with_sink(Phase::WriterRestore, false, collect));
        assert!(records().is_empty());
    }

    #[test]
    fn finish_emits_one_matching_end() {
        records();
        StartupSpan::with_sink(Phase::BindingFacts, true, collect).finish();
        let records = records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].event, Event::Begin);
        assert_eq!(records[1].event, Event::End);
        assert_eq!(records[0].span, records[1].span);
        assert!(records[1].at_ns >= records[0].at_ns);
    }

    #[test]
    fn early_return_is_incomplete_not_end() {
        fn failure() -> Result<(), ()> {
            let _span = StartupSpan::with_sink(Phase::WriterRestore, true, collect);
            Err(())
        }
        records();
        assert!(failure().is_err());
        let records = records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].event, Event::Incomplete);
    }

    #[test]
    fn unwind_is_incomplete_and_does_not_claim_success() {
        records();
        let result = std::panic::catch_unwind(|| {
            let _span = StartupSpan::with_sink(Phase::ExtensionRegister, true, collect);
            panic!("synthetic failure");
        });
        assert!(result.is_err());
        let records = records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].event, Event::Incomplete);
    }

    #[test]
    fn record_has_only_fixed_labels_and_numeric_fields() {
        let record = Record {
            phase: Phase::MetadataLookup,
            span: 17,
            event: Event::End,
            at_ns: 123,
            elapsed_ns: 99,
        };
        assert_eq!(
            record.to_string(),
            format!("RPI_STARTUP_DIAGNOSTICS pid={} span=17 phase=metadata_lookup event=end at_ns=123 elapsed_ns=99", std::process::id())
        );
    }
}
