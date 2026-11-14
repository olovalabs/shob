//! Opt-in, metadata-only timings for the agent streaming pipeline.
//!
//! Enabled with `KATHAMO_STREAM_TIMINGS=1`. When disabled every entry point is
//! one relaxed atomic load, so the hot path is unchanged. Enabled, it records
//! only durations and event counts — never message text, tool arguments or
//! tool output — and prints a percentile summary every few seconds.
//!
//! The labels are deliberately split by stage so a slow frame can be attributed
//! without a profiler:
//!
//! - `engine.*`       child process spawn and first RPC record
//! - `bridge.*`       JSON decode and event projection on the reader thread
//! - `ui.drain`       draining a bridge's channel on the UI thread
//! - `queue.*`        raw/projected admission, mutex wait and queued residence;
//!                    `.backpressure` counts only admissions that waited,
//!                    `.snapshot_dropped` is a zero-duration event counter
//! - `transcript.*`   serializing and enqueueing a history entry
//! - `ui.apply`       applying one event to the visible transcript
//! - `ui.render_rows` building chat rows in a frame
//! - `markdown.*`     initial and update parses (components crate)
//! - `tool.*`         event arrival → completion / UI apply / first card / first output
//! - `tool.output_to_paint` first nonempty snapshot at host admission → frame
//!
//! Percentiles sample the first 4,096 observations per label/window; `n` and
//! `max` include every observation. Timings perturb execution when enabled.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Which part of a streamed answer became visible first. Reasoning and answer
/// text are tracked separately so a slow first answer is not hidden by a fast
/// reasoning delta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeltaKind {
    Reasoning,
    Answer,
}

const REPORT_INTERVAL_MS: u64 = 5_000;
const MAX_SAMPLES: usize = 4_096;
const MAX_TRACKED_TOOLS: usize = 2_048;

static ENABLED: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("KATHAMO_STREAM_TIMINGS").is_some_and(|value| value != "0"));

/// `true` when stream timings are on. Cheap enough to call per event.
#[inline]
pub fn enabled() -> bool {
    *ENABLED
}

#[derive(Default)]
struct Stats {
    count: u64,
    samples: Vec<u64>,
    max: u64,
}

static STATS: LazyLock<Mutex<HashMap<&'static str, Stats>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static REPORT_ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
static NEXT_REPORT_MS: AtomicU64 = AtomicU64::new(0);

fn with_stats<R>(f: impl FnOnce(&mut HashMap<&'static str, Stats>) -> R) -> R {
    let mut stats = STATS.lock().unwrap_or_else(|error| error.into_inner());
    f(&mut stats)
}

/// Record one duration sample under `label`.
pub fn record(label: &'static str, elapsed: Duration) {
    if !enabled() {
        return;
    }
    let micros = elapsed.as_micros().min(u128::from(u32::MAX)) as u64;
    with_stats(|stats| {
        let entry = stats.entry(label).or_default();
        entry.count += 1;
        entry.max = entry.max.max(micros);
        if entry.samples.len() < MAX_SAMPLES {
            entry.samples.push(micros);
        }
    });
}

/// Record the time from `start` until now.
#[inline]
pub fn record_since(label: &'static str, start: Instant) {
    if enabled() {
        record(label, start.elapsed());
    }
}

/// Print and reset counters at most every `REPORT_INTERVAL_MS`. Call from an
/// existing periodic task; it is a no-op when disabled or not yet due.
pub fn maybe_report() {
    if !enabled() {
        return;
    }
    let elapsed = REPORT_ORIGIN.elapsed().as_millis() as u64;
    let next = NEXT_REPORT_MS.load(Ordering::Relaxed);
    if next == 0 {
        NEXT_REPORT_MS.store(elapsed + REPORT_INTERVAL_MS, Ordering::Relaxed);
        return;
    }
    if elapsed < next {
        return;
    }
    NEXT_REPORT_MS.store(elapsed + REPORT_INTERVAL_MS, Ordering::Relaxed);
    report();
}

/// Print the current window immediately, for benchmark drivers.
#[cfg_attr(not(test), allow(dead_code))]
pub fn report_now() {
    if enabled() {
        report();
    }
}

fn report() {
    let mut entries: Vec<(&'static str, Stats)> = with_stats(|stats| stats.drain().collect());
    if entries.is_empty() {
        return;
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.1.count));
    eprintln!("kathamo stream timings (us, per report window):");
    for (label, mut stats) in entries {
        stats.samples.sort_unstable();
        let percentile = |fraction: f64| -> u64 {
            if stats.samples.is_empty() {
                return 0;
            }
            let index =
                ((stats.samples.len() as f64 * fraction) as usize).min(stats.samples.len() - 1);
            stats.samples[index]
        };
        let max = stats.max;
        eprintln!(
            "  {label:<26} n={:<6} p50={:<8} p95={:<8} max={}",
            stats.count,
            percentile(0.50),
            percentile(0.95),
            max,
        );
    }
}

struct ToolTrace {
    arrived: Instant,
    ended: Option<Instant>,
    output_arrived: Option<Instant>,
    card_painted: bool,
    output_painted: bool,
}

static TOOLS: LazyLock<Mutex<HashMap<String, ToolTrace>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// Note when a tool start record was decoded. Keyed by the RPC tool id.
pub fn tool_arrived(id: &str) {
    if !enabled() {
        return;
    }
    let mut tools = lock(&TOOLS);
    if tools.len() >= MAX_TRACKED_TOOLS {
        tools.clear();
    }
    tools.insert(
        id.to_owned(),
        ToolTrace {
            arrived: Instant::now(),
            ended: None,
            output_arrived: None,
            card_painted: false,
            output_painted: false,
        },
    );
}

/// First non-empty tool snapshot reached host transport admission, before
/// queueing or projection. Final-only tool output uses the same marker.
pub fn tool_output_arrived(id: &str) {
    if !enabled() {
        return;
    }
    if let Some(tool) = lock(&TOOLS).get_mut(id) {
        tool.output_arrived.get_or_insert_with(Instant::now);
    }
}

/// Time between the start and end events reaching the bridge. Includes agent
/// preparation and finalization as well as the tool call itself.
pub fn tool_finished(id: &str) {
    if !enabled() {
        return;
    }
    let now = Instant::now();
    let arrived = lock(&TOOLS)
        .get_mut(id)
        .and_then(|tool| tool.ended.replace(now).is_none().then_some(tool.arrived));
    if let Some(arrived) = arrived {
        record("tool.start_to_end", now.saturating_duration_since(arrived));
    }
}

/// Associate the most recent tool end with the next model delta in this turn.
pub fn turn_tool_finished(trace: u64, id: &str) {
    if !enabled() {
        return;
    }
    let ended = lock(&TOOLS).get(id).and_then(|tool| tool.ended);
    if let Some(turn) = lock(&TURNS).get_mut(&trace) {
        turn.last_tool_end = ended;
    }
}

/// RPC arrival → the event was applied to the visible transcript.
pub fn tool_applied(id: &str) {
    if !enabled() {
        return;
    }
    let arrived = lock(&TOOLS).get(id).map(|trace| trace.arrived);
    if let Some(arrived) = arrived {
        record_since("tool.arrival_to_apply", arrived);
    }
}

pub fn tool_paint_due(id: &str, output: bool) -> bool {
    enabled()
        && lock(&TOOLS).get(id).is_some_and(|trace| {
            if output {
                !trace.output_painted
            } else {
                !trace.card_painted
            }
        })
}

/// RPC arrival → the tool card first entered a frame.
pub fn tool_card_painted(id: &str) {
    tool_painted(id, "tool.arrival_to_card", |trace| &mut trace.card_painted);
}

/// Tool start and first non-empty snapshot → the first output entered a frame.
pub fn tool_output_painted(id: &str) {
    if !enabled() {
        return;
    }
    let times = lock(&TOOLS).get_mut(id).and_then(|trace| {
        if std::mem::replace(&mut trace.output_painted, true) {
            None
        } else {
            Some((trace.arrived, trace.output_arrived))
        }
    });
    if let Some((arrived, output_arrived)) = times {
        let painted = Instant::now();
        record(
            "tool.arrival_to_output",
            painted.saturating_duration_since(arrived),
        );
        if let Some(output_arrived) = output_arrived {
            record(
                "tool.output_to_paint",
                painted.saturating_duration_since(output_arrived),
            );
        }
    }
}

fn tool_painted(id: &str, label: &'static str, flag: impl FnOnce(&mut ToolTrace) -> &mut bool) {
    if !enabled() {
        return;
    }
    let arrived = lock(&TOOLS).get_mut(id).and_then(|trace| {
        if std::mem::replace(flag(trace), true) {
            None
        } else {
            Some(trace.arrived)
        }
    });
    if let Some(arrived) = arrived {
        record_since(label, arrived);
    }
}

// ---------------------------------------------------------------------------
// Per-turn lifecycle
// ---------------------------------------------------------------------------

static NEXT_TRACE: AtomicU64 = AtomicU64::new(1);

/// Allocate a metadata-only turn identity for one conversation.
pub fn new_trace() -> u64 {
    NEXT_TRACE.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone)]
struct TurnTrace {
    sent: Instant,
    warm: bool,
    provider_request: Option<Instant>,
    last_tool_end: Option<Instant>,
    first: [Option<Instant>; 2],
    applied: [Option<Instant>; 2],
    painted: [bool; 2],
    cancelled: Option<Instant>,
    settled: bool,
}

impl TurnTrace {
    fn awaiting_paint(&self) -> bool {
        (0..self.first.len()).any(|index| self.applied[index].is_some() && !self.painted[index])
    }
}

static TURNS: LazyLock<Mutex<HashMap<u64, TurnTrace>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Paint identity for the current turn, including a turn that settled before
/// its first frame. A conversation trace is reused; its submit time is not.
pub fn turn_started_at(trace: u64) -> Option<Instant> {
    if !enabled() {
        return None;
    }
    lock(&TURNS).get(&trace).map(|turn| turn.sent)
}

fn phase_label(warm: bool, suffix: &str) -> &'static str {
    match (warm, suffix) {
        (true, "send_to_provider") => "turn.warm.send_to_provider",
        (false, "send_to_provider") => "turn.cold.send_to_provider",
        (true, "reasoning.send_to_first_delta") => "turn.warm.reasoning.send_to_first_delta",
        (false, "reasoning.send_to_first_delta") => "turn.cold.reasoning.send_to_first_delta",
        (true, "answer.send_to_first_delta") => "turn.warm.answer.send_to_first_delta",
        (false, "answer.send_to_first_delta") => "turn.cold.answer.send_to_first_delta",
        (true, "reasoning.delta_to_apply") => "turn.warm.reasoning.delta_to_apply",
        (false, "reasoning.delta_to_apply") => "turn.cold.reasoning.delta_to_apply",
        (true, "answer.delta_to_apply") => "turn.warm.answer.delta_to_apply",
        (false, "answer.delta_to_apply") => "turn.cold.answer.delta_to_apply",
        (true, "reasoning.send_to_paint") => "turn.warm.reasoning.send_to_paint",
        (false, "reasoning.send_to_paint") => "turn.cold.reasoning.send_to_paint",
        (true, "answer.send_to_paint") => "turn.warm.answer.send_to_paint",
        (false, "answer.send_to_paint") => "turn.cold.answer.send_to_paint",
        (true, "reasoning.delta_to_paint") => "turn.warm.reasoning.delta_to_paint",
        (false, "reasoning.delta_to_paint") => "turn.cold.reasoning.delta_to_paint",
        (true, "answer.delta_to_paint") => "turn.warm.answer.delta_to_paint",
        (false, "answer.delta_to_paint") => "turn.cold.answer.delta_to_paint",
        (true, "reasoning.apply_to_paint") => "turn.warm.reasoning.apply_to_paint",
        (false, "reasoning.apply_to_paint") => "turn.cold.reasoning.apply_to_paint",
        (true, "answer.apply_to_paint") => "turn.warm.answer.apply_to_paint",
        (false, "answer.apply_to_paint") => "turn.cold.answer.apply_to_paint",
        _ => "turn.dropped",
    }
}

fn kind_index(kind: DeltaKind) -> usize {
    match kind {
        DeltaKind::Reasoning => 0,
        DeltaKind::Answer => 1,
    }
}

/// Mark the moment a prompt was submitted for a conversation.
pub fn turn_sent(trace: u64, warm: bool) {
    if !enabled() {
        return;
    }
    let mut turns = lock(&TURNS);
    if turns.len() >= 1_024 {
        turns.clear();
    }
    turns.insert(
        trace,
        TurnTrace {
            sent: Instant::now(),
            warm,
            provider_request: None,
            last_tool_end: None,
            first: [None; 2],
            applied: [None; 2],
            painted: [false; 2],
            cancelled: None,
            settled: false,
        },
    );
}

/// The provider request was handed to the HTTP client. Fired from the
/// embedded session's provider hook, before the first response byte.
pub fn turn_provider_request(trace: u64) {
    if !enabled() {
        return;
    }
    let mut turns = lock(&TURNS);
    let Some(turn) = turns.get_mut(&trace) else {
        return;
    };
    if turn.provider_request.is_some() {
        return;
    }
    turn.provider_request = Some(Instant::now());
    let sent = turn.sent;
    let label = phase_label(turn.warm, "send_to_provider");
    drop(turns);
    record_since(label, sent);
}

/// The first non-empty delta for `kind` reached the bridge.
pub fn turn_delta_arrived(trace: u64, kind: DeltaKind) {
    if !enabled() {
        return;
    }
    let index = kind_index(kind);
    let mut turns = lock(&TURNS);
    let Some(turn) = turns.get_mut(&trace) else {
        return;
    };
    let now = Instant::now();
    let last_tool_end = turn.last_tool_end.take();
    let first = if turn.first[index].is_none() {
        turn.first[index] = Some(now);
        let label = phase_label(
            turn.warm,
            match kind {
                DeltaKind::Reasoning => "reasoning.send_to_first_delta",
                DeltaKind::Answer => "answer.send_to_first_delta",
            },
        );
        Some((label, turn.sent))
    } else {
        None
    };
    drop(turns);
    if let Some(ended) = last_tool_end {
        record(
            "tool.end_to_model_delta",
            now.saturating_duration_since(ended),
        );
    }
    if let Some((label, sent)) = first {
        record(label, now.saturating_duration_since(sent));
    }
}

/// The first delta for `kind` was applied to the visible transcript.
pub fn turn_delta_applied(trace: u64, kind: DeltaKind) {
    if !enabled() {
        return;
    }
    let index = kind_index(kind);
    let mut turns = lock(&TURNS);
    let Some(turn) = turns.get_mut(&trace) else {
        return;
    };
    if turn.applied[index].is_some() {
        return;
    }
    let Some(arrived) = turn.first[index] else {
        return;
    };
    let now = Instant::now();
    turn.applied[index] = Some(now);
    let label = phase_label(
        turn.warm,
        match kind {
            DeltaKind::Reasoning => "reasoning.delta_to_apply",
            DeltaKind::Answer => "answer.delta_to_apply",
        },
    );
    drop(turns);
    record(label, now.saturating_duration_since(arrived));
}

/// Whether the first visible delta of `kind` still needs a paint marker.
/// The caller attaches a canvas that records the sample when the row paints.
pub fn turn_paint_due(trace: u64, kind: DeltaKind) -> bool {
    if !enabled() {
        return false;
    }
    let index = kind_index(kind);
    let mut turns = lock(&TURNS);
    let Some(turn) = turns.get_mut(&trace) else {
        return false;
    };
    if turn.painted[index] || turn.first[index].is_none() {
        return false;
    }
    true
}

/// The assistant/reasoning row that first carried `kind` finished a frame.
pub fn turn_painted(trace: u64, kind: DeltaKind) {
    if !enabled() {
        return;
    }
    let index = kind_index(kind);
    let mut turns = lock(&TURNS);
    let Some(turn) = turns.get_mut(&trace) else {
        return;
    };
    if turn.painted[index] {
        return;
    }
    let Some(arrived) = turn.first[index] else {
        return;
    };
    if turn.cancelled.is_some() && turn.applied[index].is_none() {
        // A cancelled turn can settle before the first row is laid out; keep
        // the turn's cancellation sample and drop the paint sample.
        return;
    }
    turn.painted[index] = true;
    let now = Instant::now();
    let sent_label = phase_label(
        turn.warm,
        match kind {
            DeltaKind::Reasoning => "reasoning.send_to_paint",
            DeltaKind::Answer => "answer.send_to_paint",
        },
    );
    let delta_label = phase_label(
        turn.warm,
        match kind {
            DeltaKind::Reasoning => "reasoning.delta_to_paint",
            DeltaKind::Answer => "answer.delta_to_paint",
        },
    );
    let applied = turn.applied[index];
    let apply_label = phase_label(
        turn.warm,
        match kind {
            DeltaKind::Reasoning => "reasoning.apply_to_paint",
            DeltaKind::Answer => "answer.apply_to_paint",
        },
    );
    let sent = turn.sent;
    if turn.settled && !turn.awaiting_paint() {
        turns.remove(&trace);
    }
    drop(turns);
    record(sent_label, now.saturating_duration_since(sent));
    record(delta_label, now.saturating_duration_since(arrived));
    if let Some(applied) = applied {
        record(apply_label, now.saturating_duration_since(applied));
    }
}

/// The user asked to stop the turn.
pub fn turn_cancelled(trace: u64) {
    if !enabled() {
        return;
    }
    let mut turns = lock(&TURNS);
    if let Some(turn) = turns.get_mut(&trace)
        && turn.cancelled.is_none()
    {
        turn.cancelled = Some(Instant::now());
    }
}

/// The backend reported completion (or closure) for the turn.
pub fn turn_settled(trace: u64) {
    if !enabled() {
        return;
    }
    let (sent, warm, cancelled) = {
        let mut turns = lock(&TURNS);
        let Some(turn) = turns.get_mut(&trace) else {
            return;
        };
        if std::mem::replace(&mut turn.settled, true) {
            return;
        }
        let sample = (turn.sent, turn.warm, turn.cancelled);
        // Completion and the first text can be drained in the same UI batch.
        if !turn.awaiting_paint() {
            turns.remove(&trace);
        }
        sample
    };
    let label = if warm {
        "turn.warm.send_to_settle"
    } else {
        "turn.cold.send_to_settle"
    };
    record_since(label, sent);
    if let Some(cancelled) = cancelled {
        record_since("turn.cancel_to_settle", cancelled);
    }
}

/// Attach a startup phase to the current report window.
pub fn record_for_trace(_trace: u64, label: &'static str, elapsed: Duration) {
    record(label, elapsed);
}
