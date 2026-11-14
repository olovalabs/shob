//! Paint-side counters for the app's opt-in diagnostics.
//!
//! The terminal crate cannot see the app's `diagnostics` module, so it keeps
//! its own cumulative counters here and the app drains them with
//! [`take_paint_stats`] when it prints a report. When `KATHAMO_DIAGNOSTICS` is
//! unset the hooks are a single relaxed load.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static ENABLED: LazyLock<bool> = LazyLock::new(|| {
    std::env::var_os("KATHAMO_DIAGNOSTICS")
        .or_else(|| std::env::var_os("MARKUP_DIAGNOSTICS"))
        .is_some_and(|value| value != "0")
});

#[inline]
pub fn enabled() -> bool {
    *ENABLED
}

static PAINTS: AtomicU64 = AtomicU64::new(0);
static PAINT_NANOS: AtomicU64 = AtomicU64::new(0);
static PAINT_MAX_NANOS: AtomicU64 = AtomicU64::new(0);

static PARSE_BATCHES: AtomicU64 = AtomicU64::new(0);
static PARSE_NANOS: AtomicU64 = AtomicU64::new(0);
static PARSE_MAX_NANOS: AtomicU64 = AtomicU64::new(0);
static PARSE_BYTES: AtomicU64 = AtomicU64::new(0);
static PARSE_MAX_BYTES: AtomicU64 = AtomicU64::new(0);

/// Start timing one terminal paint. `None` when diagnostics are off.
#[inline]
pub fn paint_start() -> Option<Instant> {
    enabled().then(Instant::now)
}

#[inline]
pub fn paint_end(start: Option<Instant>) {
    let Some(start) = start else { return };
    let nanos = start.elapsed().as_nanos() as u64;
    PAINTS.fetch_add(1, Ordering::Relaxed);
    PAINT_NANOS.fetch_add(nanos, Ordering::Relaxed);
    PAINT_MAX_NANOS.fetch_max(nanos, Ordering::Relaxed);
}

/// Count, cumulative nanos and worst-case nanos since the previous call.
pub struct PaintStats {
    pub paints: u64,
    pub nanos: u64,
    pub max_nanos: u64,
}

pub fn take_paint_stats() -> PaintStats {
    PaintStats {
        paints: PAINTS.swap(0, Ordering::Relaxed),
        nanos: PAINT_NANOS.swap(0, Ordering::Relaxed),
        max_nanos: PAINT_MAX_NANOS.swap(0, Ordering::Relaxed),
    }
}

/// Start timing one PTY parse batch on the UI thread.
#[inline]
pub fn parse_start() -> Option<Instant> {
    enabled().then(Instant::now)
}

#[inline]
pub fn parse_end(start: Option<Instant>, bytes: usize) {
    let Some(start) = start else { return };
    let nanos = start.elapsed().as_nanos() as u64;
    PARSE_BATCHES.fetch_add(1, Ordering::Relaxed);
    PARSE_NANOS.fetch_add(nanos, Ordering::Relaxed);
    PARSE_MAX_NANOS.fetch_max(nanos, Ordering::Relaxed);
    PARSE_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    PARSE_MAX_BYTES.fetch_max(bytes as u64, Ordering::Relaxed);
}

/// Batches, cumulative nanos, worst case, bytes and largest batch since the
/// previous call.
pub struct ParseStats {
    pub batches: u64,
    pub nanos: u64,
    pub max_nanos: u64,
    pub bytes: u64,
    pub max_bytes: u64,
}

pub fn take_parse_stats() -> ParseStats {
    ParseStats {
        batches: PARSE_BATCHES.swap(0, Ordering::Relaxed),
        nanos: PARSE_NANOS.swap(0, Ordering::Relaxed),
        max_nanos: PARSE_MAX_NANOS.swap(0, Ordering::Relaxed),
        bytes: PARSE_BYTES.swap(0, Ordering::Relaxed),
        max_bytes: PARSE_MAX_BYTES.swap(0, Ordering::Relaxed),
    }
}

static QUEUED_BATCHES: AtomicU64 = AtomicU64::new(0);
static QUEUE_NANOS: AtomicU64 = AtomicU64::new(0);
static QUEUE_MAX_NANOS: AtomicU64 = AtomicU64::new(0);
static MAX_QUEUED_CHUNKS: AtomicU64 = AtomicU64::new(0);
static PAINTED_BATCHES: AtomicU64 = AtomicU64::new(0);
static READ_TO_PAINT_NANOS: AtomicU64 = AtomicU64::new(0);
static READ_TO_PAINT_MAX_NANOS: AtomicU64 = AtomicU64::new(0);
static PARSE_TO_PAINT_NANOS: AtomicU64 = AtomicU64::new(0);
static PARSE_TO_PAINT_MAX_NANOS: AtomicU64 = AtomicU64::new(0);

/// Oldest output awaiting a paint of this particular terminal, not another pane.
pub(crate) struct PendingOutput {
    read_at: Instant,
    parsed_at: Instant,
}

#[inline]
pub(crate) fn output_received() -> Option<Instant> {
    enabled().then(Instant::now)
}

pub(crate) fn output_queued(read_at: Option<Instant>, queued_chunks: usize) {
    let Some(read_at) = read_at else { return };
    let nanos = read_at.elapsed().as_nanos() as u64;
    QUEUED_BATCHES.fetch_add(1, Ordering::Relaxed);
    QUEUE_NANOS.fetch_add(nanos, Ordering::Relaxed);
    QUEUE_MAX_NANOS.fetch_max(nanos, Ordering::Relaxed);
    MAX_QUEUED_CHUNKS.fetch_max(queued_chunks as u64, Ordering::Relaxed);
}

pub(crate) fn output_parsed(
    pending: &parking_lot::Mutex<Option<PendingOutput>>,
    read_at: Option<Instant>,
) {
    let Some(read_at) = read_at else { return };
    pending.lock().get_or_insert_with(|| PendingOutput {
        read_at,
        parsed_at: Instant::now(),
    });
}

pub(crate) fn output_painted(pending: &parking_lot::Mutex<Option<PendingOutput>>) {
    if !enabled() {
        return;
    }
    let Some(output) = pending.lock().take() else {
        return;
    };
    let now = Instant::now();
    let read_nanos = now.duration_since(output.read_at).as_nanos() as u64;
    let parse_nanos = now.duration_since(output.parsed_at).as_nanos() as u64;
    PAINTED_BATCHES.fetch_add(1, Ordering::Relaxed);
    READ_TO_PAINT_NANOS.fetch_add(read_nanos, Ordering::Relaxed);
    READ_TO_PAINT_MAX_NANOS.fetch_max(read_nanos, Ordering::Relaxed);
    PARSE_TO_PAINT_NANOS.fetch_add(parse_nanos, Ordering::Relaxed);
    PARSE_TO_PAINT_MAX_NANOS.fetch_max(parse_nanos, Ordering::Relaxed);
}

/// Metadata-only output latency, aggregated across views. Queue time starts
/// after Read completes, including bounded-channel backpressure and coalescing.
/// Paint samples are the oldest pending output per painted view; hidden views
/// intentionally retain that timestamp until they become visible again.
pub struct OutputStats {
    pub queued_batches: u64,
    pub queue_nanos: u64,
    pub queue_max_nanos: u64,
    pub max_queued_chunks: u64,
    pub painted_batches: u64,
    pub read_to_paint_nanos: u64,
    pub read_to_paint_max_nanos: u64,
    pub parse_to_paint_nanos: u64,
    pub parse_to_paint_max_nanos: u64,
}

pub fn take_output_stats() -> OutputStats {
    OutputStats {
        queued_batches: QUEUED_BATCHES.swap(0, Ordering::Relaxed),
        queue_nanos: QUEUE_NANOS.swap(0, Ordering::Relaxed),
        queue_max_nanos: QUEUE_MAX_NANOS.swap(0, Ordering::Relaxed),
        max_queued_chunks: MAX_QUEUED_CHUNKS.swap(0, Ordering::Relaxed),
        painted_batches: PAINTED_BATCHES.swap(0, Ordering::Relaxed),
        read_to_paint_nanos: READ_TO_PAINT_NANOS.swap(0, Ordering::Relaxed),
        read_to_paint_max_nanos: READ_TO_PAINT_MAX_NANOS.swap(0, Ordering::Relaxed),
        parse_to_paint_nanos: PARSE_TO_PAINT_NANOS.swap(0, Ordering::Relaxed),
        parse_to_paint_max_nanos: PARSE_TO_PAINT_MAX_NANOS.swap(0, Ordering::Relaxed),
    }
}
