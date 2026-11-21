//! Headless grid-memory baseline for the session lifecycle.
//!
//! Runs without a display: builds `TerminalState`s directly, feeds agent-like
//! output, and reports alacritty's accounted grid bytes next to the process
//! RSS. PTY threads are not started here, so this measures the per-session
//! allocation that dominates an idle workspace; thread counts come from the app
//! under `SHOB_DIAGNOSTICS=1`.
//!
//! ```text
//! cargo run --release -p terminal --example bench_memory -- 1 10 50
//! ```
//!
//! Each argument is a session count, measured in one process. The previous
//! sweep is dropped (and, after resetting the grids, the allocations are
//! returned) before the next one is built, so the numbers are comparable.

use std::sync::mpsc::channel;
use terminal::{GpuiEventProxy, TerminalState};

const COLUMNS: usize = 120;
const ROWS: usize = 24;
const SCROLLBACK: usize = 10_000;
const LINES_PER_SESSION: usize = 10_000;
const SEMANTIC_ESCAPE_CHARS: &str = ",│`|:\"' ()[]{}<>\t";

fn rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse().ok())
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Roughly the shape of an agent CLI's output: a coloured bullet, a path and a
/// number, wrapped to more than one line at 120 columns.
fn agent_output(lines: usize) -> Vec<u8> {
    let mut out = String::with_capacity(lines * 96);
    for line in 0..lines {
        out.push_str(&format!(
            "\x1b[32m●\x1b[0m read src/module_{}/file.rs:{} ({} bytes)\r\n",
            line % 64,
            line,
            32 + (line * 7) % 4096
        ));
    }
    out.into_bytes()
}

fn build() -> TerminalState {
    let (tx, _rx) = channel();
    TerminalState::with_config(
        COLUMNS,
        ROWS,
        TerminalState::build_config(SCROLLBACK, SEMANTIC_ESCAPE_CHARS, false),
        GpuiEventProxy::new(tx),
    )
}

fn feed(state: &mut TerminalState, bytes: &[u8]) {
    for piece in bytes.chunks(128 * 1024) {
        state.feed_raw(piece);
    }
}

fn main() {
    let mut counts: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|argument| argument.parse().ok())
        .collect();
    if counts.is_empty() {
        counts = vec![1, 10, 50];
    }
    let output = agent_output(LINES_PER_SESSION);
    let start_rss = rss_kib();

    println!(
        "{}x{} cells, {} lines of scrollback, {} lines of output per session",
        COLUMNS, ROWS, SCROLLBACK, LINES_PER_SESSION
    );
    for count in counts {
        let before = rss_kib();
        let mut sessions = Vec::with_capacity(count);
        for _ in 0..count {
            let mut state = build();
            feed(&mut state, &output);
            sessions.push(state);
        }

        let accounted: usize = sessions
            .iter()
            .map(|state| state.memory_usage().grid_bytes)
            .sum();
        let reserved: usize = sessions
            .iter()
            .map(|state| state.memory_usage().reserved_bytes)
            .sum();
        let history: usize = sessions
            .iter()
            .map(|state| state.memory_usage().history_lines)
            .sum();
        let loaded = rss_kib();

        // Hibernation: keep the text (not measured here: the caller holds it)
        // and drop every grid.
        let freed: usize = sessions
            .iter_mut()
            .map(|state| state.reset_grid(0).grid_bytes)
            .sum();
        let after_reset = rss_kib();

        println!(
            "sessions={count} history_lines={} accounted={:.1} MiB reserved<={:.1} MiB \
             rss={:.1} MiB (+{:.1}) after_hibernate_accounted={:.1} MiB rss={:.1} MiB",
            history,
            mib(accounted as u64),
            mib(reserved as u64),
            loaded.map(mib).unwrap_or(f64::NAN),
            match (before, loaded) {
                (Some(before), Some(loaded)) => mib(loaded.saturating_sub(before)),
                _ => f64::NAN,
            },
            mib(freed as u64),
            after_reset.map(mib).unwrap_or(f64::NAN),
        );

        drop(sessions);
    }

    println!(
        "process start rss={:.1} MiB end rss={:.1} MiB",
        start_rss.map(mib).unwrap_or(f64::NAN),
        rss_kib().map(mib).unwrap_or(f64::NAN)
    );
    println!(
        "RSS can stay above the accounted heap after a drop: glibc keeps freed \
         large blocks in its arenas. The app calls malloc_trim after hibernation."
    );
}
