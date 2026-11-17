//! Opt-in numeric-only recovery timing. Does not read or copy session payloads.
use std::{io::Write, time::Instant};

pub(crate) struct RecoveryPhase {
    phase: &'static str,
    started: Option<Instant>,
}

impl RecoveryPhase {
    pub(crate) fn start(phase: &'static str) -> Self {
        Self {
            phase,
            started: std::env::var_os("RPI_SESSION_DIAGNOSTICS")
                .filter(|value| value != "0")
                .map(|_| Instant::now()),
        }
    }

    /// `input` and `output` units are documented per phase. Emit only on success;
    /// absence of a phase is not evidence of a completed recovery.
    pub(crate) fn finish(self, input: usize, output: usize) {
        if let Some(started) = self.started {
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(
                stderr,
                "RPI_SESSION_DIAGNOSTICS phase={} elapsed_ns={} input={} output={}",
                self.phase,
                started.elapsed().as_nanos().min(u64::MAX as u128),
                input,
                output,
            );
        }
    }
}
