//! Per-turn token and cache telemetry.
//!
//! One row per provider turn, fed from `AgentEvent::Usage` (already forwarded by
//! `src/agent_bridge.rs`) plus the cache counters. The hit rate is the same
//! quantity `agent/crates/harness/src/cache_stats.rs` prices as waste, expressed
//! as a ratio: `cache_read / (cache_read + input)`. `cache_write` is excluded
//! from the denominator because those tokens are paid for, not re-read.
//!
//! Rows are appended as JSONL and also kept in a bounded in-memory ring so the UI
//! can read a summary without touching disk.

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::cache::CacheCounters;

/// One turn's numbers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnRow {
    pub turn: u64,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    /// `cache_read / (cache_read + input)`; `0.0` before any turn reports usage.
    pub cache_hit_rate: f64,
    /// Bytes the cache and minimizer kept out of this turn's prompt.
    pub bytes_saved: u64,
    pub tool_calls: u64,
    /// True when the cached prefix digest changed without a tool-set change.
    pub prefix_drift: bool,
}

/// Cumulative session totals plus the most recent rows.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub turns: u64,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub bytes_saved: u64,
    /// Weighted by tokens, not the mean of per-turn rates.
    pub cache_hit_rate: f64,
    pub rows: Vec<TurnRow>,
}

impl Summary {
    pub fn record(&mut self, row: TurnRow) {
        self.turns += 1;
        self.input += row.input;
        self.output += row.output;
        self.cache_read += row.cache_read;
        self.cache_write += row.cache_write;
        self.bytes_saved += row.bytes_saved;
        self.cache_hit_rate = hit_rate(self.cache_read, self.input);
        self.rows.push(row);
    }

    /// The last `count` rows, oldest first.
    pub fn tail(&self, count: usize) -> &[TurnRow] {
        let start = self.rows.len().saturating_sub(count);
        &self.rows[start..]
    }
}

/// Token-weighted cache hit rate over the session's re-readable prompt tokens.
pub fn hit_rate(cache_read: i64, input: i64) -> f64 {
    let denominator = cache_read.max(0) + input.max(0);
    if denominator == 0 {
        return 0.0;
    }
    cache_read.max(0) as f64 / denominator as f64
}

/// Where rows go. `None` = in-memory only.
pub struct MetricsSink {
    path: Option<PathBuf>,
    summary: Summary,
    recent: VecDeque<TurnRow>,
    ring: usize,
}

impl MetricsSink {
    pub fn in_memory(ring: usize) -> Self {
        Self {
            path: None,
            summary: Summary::default(),
            recent: VecDeque::with_capacity(ring),
            ring,
        }
    }

    /// Append to `path` as well as memory. The file is opened per write so a
    /// long session does not hold a descriptor open, and a failed write never
    /// disturbs the turn it is measuring.
    pub fn with_file(path: impl Into<PathBuf>, ring: usize) -> Self {
        Self {
            path: Some(path.into()),
            ..Self::in_memory(ring)
        }
    }

    pub fn record(&mut self, row: TurnRow) {
        self.summary.record(row);
        if self.ring > 0 {
            if self.recent.len() == self.ring {
                self.recent.pop_front();
            }
            self.recent.push_back(row);
        }
        self.append(row);
    }

    fn append(&self, row: TurnRow) {
        let Some(path) = &self.path else {
            return;
        };
        let Ok(line) = serde_json::to_string(&row) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{line}");
        }
    }

    pub fn summary(&self) -> &Summary {
        &self.summary
    }

    pub fn recent(&self) -> &VecDeque<TurnRow> {
        &self.recent
    }

    /// Build a row from the provider's usage numbers plus the bytes this turn
    /// actually kept out of the prompt. `counters` is accepted so callers can
    /// pass the live cache state without a second lock acquisition at the call
    /// site; the per-turn figure is what belongs in the row, since the cumulative
    /// total would double-count across turns.
    #[allow(clippy::too_many_arguments)]
    pub fn row(
        turn: u64,
        input: i64,
        output: i64,
        cache_read: i64,
        cache_write: i64,
        _counters: &CacheCounters,
        bytes_saved_this_turn: u64,
        tool_calls: u64,
        prefix_drift: bool,
    ) -> TurnRow {
        TurnRow {
            turn,
            input,
            output,
            cache_read,
            cache_write,
            cache_hit_rate: hit_rate(cache_read, input),
            bytes_saved: bytes_saved_this_turn,
            tool_calls,
            prefix_drift,
        }
    }

    /// Default location under the Shob data dir, alongside `sessions/`.
    pub fn default_path(data_dir: &Path) -> PathBuf {
        data_dir.join("token-efficiency").join("turns.jsonl")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(turn: u64, input: i64, cache_read: i64) -> TurnRow {
        TurnRow {
            turn,
            input,
            output: 10,
            cache_read,
            cache_write: 0,
            cache_hit_rate: hit_rate(cache_read, input),
            bytes_saved: 0,
            tool_calls: 1,
            prefix_drift: false,
        }
    }

    #[test]
    fn hit_rate_is_a_ratio_of_re_readable_tokens() {
        assert_eq!(hit_rate(0, 0), 0.0);
        assert_eq!(hit_rate(100, 0), 1.0);
        assert_eq!(hit_rate(0, 100), 0.0);
        assert!((hit_rate(75, 25) - 0.75).abs() < 1e-9);
    }

    #[test]
    fn hit_rate_ignores_negative_usage() {
        assert_eq!(hit_rate(-5, 0), 0.0);
        assert_eq!(
            hit_rate(10, -10),
            1.0,
            "a negative denominator is clamped to zero"
        );
    }

    #[test]
    fn summary_totals_and_weights_the_rate() {
        let mut summary = Summary::default();
        summary.record(row(1, 100, 900));
        summary.record(row(2, 100, 900));
        assert_eq!(summary.turns, 2);
        assert_eq!(summary.input, 200);
        assert_eq!(summary.cache_read, 1800);
        // Weighted: 1800 / 2000, not the mean of 0.9 and 0.9 (same here, but
        // the next assertion is where they would differ).
        assert!((summary.cache_hit_rate - 0.9).abs() < 1e-9);

        let mut skewed = Summary::default();
        skewed.record(row(1, 10, 0)); // rate 0.0
        skewed.record(row(2, 1000, 9000)); // rate 0.9
        // Mean of rates would be 0.45; the token-weighted rate is 9000/10010.
        assert!((skewed.cache_hit_rate - 9000.0 / 10010.0).abs() < 1e-9);
        assert!(skewed.cache_hit_rate > 0.85);
    }

    #[test]
    fn tail_returns_the_most_recent_rows() {
        let mut summary = Summary::default();
        for turn in 1..=5 {
            summary.record(row(turn, 100, 0));
        }
        let tail = summary.tail(2);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].turn, 4);
        assert_eq!(tail[1].turn, 5);
        assert_eq!(summary.tail(99).len(), 5);
        assert_eq!(summary.tail(0).len(), 0);
    }

    #[test]
    fn ring_is_bounded() {
        let mut sink = MetricsSink::in_memory(3);
        for turn in 1..=10 {
            sink.record(row(turn, 10, 90));
        }
        assert_eq!(sink.recent().len(), 3);
        assert_eq!(sink.recent().back().unwrap().turn, 10);
        assert_eq!(sink.summary().turns, 10, "the summary is not bounded");
    }

    #[test]
    fn rows_are_appended_as_jsonl() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("turns.jsonl");
        let mut sink = MetricsSink::with_file(&path, 8);
        sink.record(row(1, 100, 0));
        sink.record(row(2, 100, 900));

        let text = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed: TurnRow = serde_json::from_str(lines[1]).expect("parse row");
        assert_eq!(parsed.turn, 2);
        assert!((parsed.cache_hit_rate - 0.9).abs() < 1e-9);
    }

    #[test]
    fn a_missing_directory_is_created_for_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("deeper").join("turns.jsonl");
        let mut sink = MetricsSink::with_file(&path, 4);
        sink.record(row(1, 1, 1));
        assert!(path.exists());
    }

    #[test]
    fn a_bad_path_does_not_panic_or_disturb_the_summary() {
        let mut sink = MetricsSink::with_file("/proc/definitely-not-writable/x.jsonl", 4);
        sink.record(row(1, 10, 90));
        assert_eq!(
            sink.summary().turns,
            1,
            "measurement survives a failed write"
        );
    }

    #[test]
    fn default_path_lives_under_the_data_dir() {
        let path = MetricsSink::default_path(Path::new("/data/shob"));
        assert_eq!(path, Path::new("/data/shob/token-efficiency/turns.jsonl"));
    }

    #[test]
    fn rows_round_trip_through_json() {
        let row = row(3, 50, 450);
        let json = serde_json::to_string(&row).expect("serialize");
        let back: TurnRow = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, row);
    }
}
