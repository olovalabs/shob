//! Opt-in runtime diagnostics for frame timing, UI-thread work, process CPU,
//! memory and wakeups.
//!
//! Enabled with `KATHAMO_DIAGNOSTICS=1`. Disabled by default, and when disabled
//! every hook is a single relaxed atomic load — there is no sampling task and
//! no per-frame syscall. When enabled, one task prints a line per second from
//! `/proc/self`, which is coarse enough not to perturb what it measures.
//!
//! The point is to answer "where did the time go" without a profiler:
//! `render` is the per-frame cost of the root view, `paint` is the terminal
//! renderer's CPU cost, `poll` is the status tick, and `fg` is the `/proc`
//! inspection that tick used to do inline.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static ENABLED: LazyLock<bool> = LazyLock::new(|| {
    std::env::var_os("KATHAMO_DIAGNOSTICS")
        .or_else(|| std::env::var_os("MARKUP_DIAGNOSTICS"))
        .is_some_and(|value| value != "0")
});

/// `true` when diagnostics are on. Cheap enough to call on every hook.
#[inline]
pub fn enabled() -> bool {
    *ENABLED
}

/// Monotonic clock origin, so timestamps fit in atomics.
static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);

#[inline]
fn now_nanos() -> u64 {
    ORIGIN.elapsed().as_nanos() as u64
}

macro_rules! counters {
    ($($name:ident),+ $(,)?) => {
        $(static $name: AtomicU64 = AtomicU64::new(0);)+
    };
}

counters!(
    FRAMES,
    RENDER_NANOS,
    RENDER_MAX_NANOS,
    POLL_TICKS,
    POLL_NANOS,
    POLL_MAX_NANOS,
    POLL_PANES,
    FG_TICKS,
    FG_NANOS,
    FG_MAX_NANOS,
    SCRAPE_NANOS,
    DETECT_NANOS,
    LAST_FRAME_AT,
    MAX_FRAME_GAP_NANOS,
);

#[inline]
fn add(counter: &AtomicU64, value: u64) {
    counter.fetch_add(value, Ordering::Relaxed);
}

#[inline]
fn bump_max(counter: &AtomicU64, value: u64) {
    counter.fetch_max(value, Ordering::Relaxed);
}

/// Start timing a root-view render. `None` when diagnostics are off.
#[inline]
pub fn render_start() -> Option<Instant> {
    enabled().then(Instant::now)
}

/// Record one root-view render. Also derives the inter-frame gap.
#[inline]
pub fn render_end(start: Option<Instant>) {
    let Some(start) = start else { return };
    let elapsed = start.elapsed();
    let now = now_nanos();
    add(&FRAMES, 1);
    add(&RENDER_NANOS, elapsed.as_nanos() as u64);
    bump_max(&RENDER_MAX_NANOS, elapsed.as_nanos() as u64);
    let previous = LAST_FRAME_AT.swap(now, Ordering::Relaxed);
    if previous != 0 {
        bump_max(&MAX_FRAME_GAP_NANOS, now.saturating_sub(previous));
    }
}

/// Start timing one status poll tick.
#[inline]
pub fn poll_start() -> Option<Instant> {
    enabled().then(Instant::now)
}

#[inline]
pub fn poll_end(start: Option<Instant>) {
    let Some(start) = start else { return };
    let elapsed = start.elapsed();
    add(&POLL_TICKS, 1);
    add(&POLL_NANOS, elapsed.as_nanos() as u64);
    bump_max(&POLL_MAX_NANOS, elapsed.as_nanos() as u64);
}

/// Record how many panes the poll pass visited, so the report scales visibly
/// with pane count.
#[inline]
pub fn panes_polled(count: u64) {
    if enabled() {
        add(&POLL_PANES, count);
    }
}

/// Start timing foreground-process inspection for one pane.
#[inline]
pub fn foreground_start() -> Option<Instant> {
    enabled().then(Instant::now)
}

#[inline]
pub fn foreground_end(start: Option<Instant>) {
    let Some(start) = start else { return };
    let elapsed = start.elapsed();
    add(&FG_TICKS, 1);
    add(&FG_NANOS, elapsed.as_nanos() as u64);
    bump_max(&FG_MAX_NANOS, elapsed.as_nanos() as u64);
}

/// Start timing a screen scrape.
#[inline]
pub fn scrape_start() -> Option<Instant> {
    enabled().then(Instant::now)
}

#[inline]
pub fn scrape_end(start: Option<Instant>) {
    if let Some(start) = start {
        add(&SCRAPE_NANOS, start.elapsed().as_nanos() as u64);
    }
}

/// Start timing detection (join + regex + rules) for one pane.
#[inline]
pub fn detect_start() -> Option<Instant> {
    enabled().then(Instant::now)
}

#[inline]
pub fn detect_end(start: Option<Instant>) {
    if let Some(start) = start {
        add(&DETECT_NANOS, start.elapsed().as_nanos() as u64);
    }
}

/// Seconds between reports. One line per second is coarse enough to observe
/// without adding a high-frequency loop.
pub const REPORT_INTERVAL: Duration = Duration::from_secs(1);

/// Owns the deltas between reports so the printed line describes the last
/// second rather than the whole run.
pub struct Sampler {
    last: Instant,
    last_cpu_ticks: u64,
    last_ctxt: u64,
    clock_ticks_per_sec: f64,
    page_size: u64,
}

impl Sampler {
    pub fn new() -> Self {
        let (cpu_ticks, _) = read_cpu_ticks().unwrap_or((0, 0));
        let ctxt = read_context_switches().unwrap_or(0);
        Self {
            last: Instant::now(),
            last_cpu_ticks: cpu_ticks,
            last_ctxt: ctxt,
            clock_ticks_per_sec: clock_ticks_per_sec(),
            page_size: page_size(),
        }
    }

    /// Build the report line for the window since the previous call.
    pub fn report(&mut self) -> String {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;

        let frames = take(&FRAMES);
        let render_nanos = take(&RENDER_NANOS);
        let render_max = take(&RENDER_MAX_NANOS);
        let paint_stats = terminal::diagnostics::take_paint_stats();
        let paints = paint_stats.paints;
        let paint_nanos = paint_stats.nanos;
        let paint_max = paint_stats.max_nanos;
        let parse_stats = terminal::diagnostics::take_parse_stats();
        let parse_batches = parse_stats.batches;
        let parse_nanos = parse_stats.nanos;
        let parse_max = parse_stats.max_nanos;
        let parse_bytes = parse_stats.bytes;
        let parse_max_bytes = parse_stats.max_bytes;
        let poll_ticks = take(&POLL_TICKS);
        let output = terminal::diagnostics::take_output_stats();
        let poll_nanos = take(&POLL_NANOS);
        let poll_max = take(&POLL_MAX_NANOS);
        let poll_panes = take(&POLL_PANES);
        let fg_ticks = take(&FG_TICKS);
        let fg_nanos = take(&FG_NANOS);
        let fg_max = take(&FG_MAX_NANOS);
        let scrape_nanos = take(&SCRAPE_NANOS);
        let detect_nanos = take(&DETECT_NANOS);
        let gap_max = take(&MAX_FRAME_GAP_NANOS);

        let cpu_ticks = read_cpu_ticks()
            .map(|(utime, stime)| utime + stime)
            .unwrap_or(0);
        let cpu_delta = cpu_ticks.saturating_sub(self.last_cpu_ticks);
        self.last_cpu_ticks = cpu_ticks;
        let cpu_percent = if elapsed > 0.0 {
            cpu_delta as f64 / self.clock_ticks_per_sec / elapsed * 100.0
        } else {
            0.0
        };

        let ctxt = read_context_switches().unwrap_or(0);
        let ctxt_delta = ctxt.saturating_sub(self.last_ctxt);
        self.last_ctxt = ctxt;

        let rss_bytes = read_rss_bytes().unwrap_or(0) * self.page_size;
        let threads = read_thread_count().unwrap_or(0);

        let avg = |total: u64, count: u64| {
            if count == 0 {
                0.0
            } else {
                total as f64 / count as f64 / 1_000_000.0
            }
        };

        format!(
            "[diag] {elapsed:.2}s frames={frames} ({fps:.1} fps, max gap {gap:.1}ms) \
             render avg={render_avg:.2}ms max={render_max:.2}ms | \
             paint avg={paint_avg:.2}ms max={paint_max:.2}ms | \
             parse batches={parse_batches} avg={parse_avg:.2}ms max={parse_max:.2}ms \
             max batch={parse_max_kb:.0}KB ({parse_mbs:.0}MB/s) | \
             pty queue max={output_queue_max:.2}ms chunks={output_backlog} \
             read→paint avg={output_paint_avg:.2}ms max={output_paint_max:.2}ms \
             parse→paint max={parse_paint_max:.2}ms | \
             poll ticks={poll_ticks} panes={poll_panes} avg={poll_avg:.3}ms max={poll_max:.3}ms \
             (scrape {scrape_avg:.3}ms, detect {detect_avg:.3}ms) | \
             fg {fg_ticks} avg={fg_avg:.3}ms max={fg_max:.3}ms | \
             cpu={cpu_percent:.1}% rss={rss:.1}MB threads={threads} ctxt={ctxt_delta}/s",
            fps = frames as f64 / elapsed.max(f64::EPSILON),
            gap = gap_max as f64 / 1_000_000.0,
            render_avg = avg(render_nanos, frames),
            render_max = render_max as f64 / 1_000_000.0,
            paint_avg = avg(paint_nanos, paints),
            paint_max = paint_max as f64 / 1_000_000.0,
            parse_avg = avg(parse_nanos, parse_batches),
            parse_max = parse_max as f64 / 1_000_000.0,
            parse_max_kb = parse_max_bytes as f64 / 1024.0,
            parse_mbs = if elapsed > 0.0 {
                parse_bytes as f64 / (1024.0 * 1024.0) / elapsed
            } else {
                0.0
            },
            output_queue_max = output.queue_max_nanos as f64 / 1_000_000.0,
            output_backlog = output.max_queued_chunks,
            output_paint_avg = avg(output.read_to_paint_nanos, output.painted_batches),
            output_paint_max = output.read_to_paint_max_nanos as f64 / 1_000_000.0,
            parse_paint_max = output.parse_to_paint_max_nanos as f64 / 1_000_000.0,
            poll_avg = avg(poll_nanos, poll_ticks),
            poll_max = poll_max as f64 / 1_000_000.0,
            scrape_avg = avg(scrape_nanos, poll_ticks),
            detect_avg = avg(detect_nanos, poll_ticks),
            fg_avg = avg(fg_nanos, fg_ticks),
            fg_max = fg_max as f64 / 1_000_000.0,
            rss = rss_bytes as f64 / (1024.0 * 1024.0),
        )
    }
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

/// Read and reset a cumulative counter (the report describes deltas).
#[inline]
fn take(counter: &AtomicU64) -> u64 {
    counter.swap(0, Ordering::Relaxed)
}

#[cfg(target_os = "linux")]
fn clock_ticks_per_sec() -> f64 {
    // SAFETY: sysconf is thread-safe and has no preconditions here.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks > 0 { ticks as f64 } else { 100.0 }
}

#[cfg(not(target_os = "linux"))]
fn clock_ticks_per_sec() -> f64 {
    100.0
}

#[cfg(target_os = "linux")]
fn page_size() -> u64 {
    // SAFETY: sysconf is thread-safe and has no preconditions here.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 { size as u64 } else { 4096 }
}

#[cfg(not(target_os = "linux"))]
fn page_size() -> u64 {
    4096
}

/// `(utime, stime)` in clock ticks from `/proc/self/stat`.
#[cfg(target_os = "linux")]
fn read_cpu_ticks() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // Slice starts at field 3 (state): utime is field 14, stime field 15.
    let utime = fields.get(11)?.parse().ok()?;
    let stime = fields.get(12)?.parse().ok()?;
    Some((utime, stime))
}

#[cfg(not(target_os = "linux"))]
fn read_cpu_ticks() -> Option<(u64, u64)> {
    None
}

#[cfg(target_os = "linux")]
fn read_context_switches() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let mut total = 0u64;
    for line in status.lines() {
        if let Some(value) = line
            .strip_prefix("voluntary_ctxt_switches:")
            .or_else(|| line.strip_prefix("involuntary_ctxt_switches:"))
        {
            total += value.trim().parse().unwrap_or(0);
        }
    }
    Some(total)
}

#[cfg(not(target_os = "linux"))]
fn read_context_switches() -> Option<u64> {
    None
}

/// Resident pages from `/proc/self/stat` field 24 (rss).
#[cfg(target_os = "linux")]
fn read_rss_bytes() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // Slice starts at field 3 (state): rss is field 24.
    fields.get(21)?.parse().ok()
}

#[cfg(target_os = "linux")]
fn read_thread_count() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // Slice starts at field 3 (state): num_threads is field 20.
    fields.get(17)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
fn read_rss_bytes() -> Option<u64> {
    None
}

#[cfg(not(target_os = "linux"))]
fn read_thread_count() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_are_no_ops_when_disabled() {
        // The global flag follows the environment, so this only checks that the
        // hooks are safe to call; `enabled()` itself is exercised by the app.
        let _ = render_start();
        let _ = poll_start();
        let _ = foreground_start();
        let _ = scrape_start();
        let _ = detect_start();
    }

    #[test]
    fn sampler_reports_without_panicking() {
        let mut sampler = Sampler::new();
        let report = sampler.report();
        assert!(report.starts_with("[diag]"));
        assert!(report.contains("rss="));
        assert!(report.contains("cpu="));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_self_fields_parse() {
        let (utime, stime) = read_cpu_ticks().expect("/proc/self/stat");
        // A very fresh process may not have accrued a tick yet, so just make
        // sure the fields parsed rather than that they are non-zero.
        let _ = utime + stime;
        assert!(read_rss_bytes().is_some_and(|pages| pages > 0));
        assert!(read_thread_count().is_some_and(|threads| threads > 0));
        assert!(read_context_switches().is_some());
    }
}
