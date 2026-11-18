//! Head+tail window cost: cargo run -p tools --example bench_window -- 200
//!
//! Reports raw microsecond samples; it asserts nothing about wall-clock
//! performance. Run it on the same machine before and after a change.
//!
//! It measures the two quantities the failure-window feature can plausibly affect,
//! with no shell, no I/O and no inference:
//!
//! 1. `capture` — retaining the 6 KiB head prefix while a large output streams in.
//!    This is the only work added to the hot path, and it stops after 6 KiB.
//! 2. `window` — building the head+tail window once, after the command failed.
//!
//! Both are reported as per-sample microseconds plus a best/worst spread so a
//! regression is visible rather than asserted.

use std::time::{Duration, Instant};

use tools::output_window::{build_window, OutputTotals, DEFAULT_HEAD_SHARE};
use tools::truncate::{TruncationOptions, DEFAULT_MAX_BYTES};

const MAX_HEAD_BYTES: usize = DEFAULT_MAX_BYTES / DEFAULT_HEAD_SHARE;
const MAX_TAIL_BYTES: usize = DEFAULT_MAX_BYTES * 2;

fn micros(time: Duration) -> f64 {
    time.as_secs_f64() * 1e6
}

fn spread(label: &str, samples: &[f64]) {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;
    println!(
        "{label}: mean {mean:.2}us  best {:.2}us  worst {:.2}us  (n={})",
        sorted[0],
        sorted[sorted.len() - 1],
        sorted.len()
    );
}

/// Mirror of the capture layer's head retention: append, and once the cap is
/// exceeded snap down to the last complete line and stop for good.
fn ingest(state: (&mut String, &mut bool), text: &str) {
    let (head, full) = state;
    if *full {
        return;
    }
    head.push_str(text);
    if head.len() > MAX_HEAD_BYTES {
        let mut cut = MAX_HEAD_BYTES;
        while cut > 0 && !head.is_char_boundary(cut) {
            cut -= 1;
        }
        let keep = match head[..cut].rfind('\n') {
            Some(newline) => newline + 1,
            None => 0,
        };
        head.truncate(keep);
        head.shrink_to_fit();
        *full = true;
    }
}

fn main() {
    let samples = std::env::args()
        .nth(1)
        .map(|n| n.parse::<usize>().unwrap())
        .unwrap_or(200)
        .clamp(1, 1_000);
    // 8 MiB of realistic log lines, in 8 KiB pipe chunks.
    let line = "error[E0308]: mismatched types --> src/main.rs:12:5 expected `u32`, found `&str`\n";
    let chunk: String = line.repeat(8192 / line.len().max(1));
    let chunks: Vec<&str> = (0..1024).map(|_| chunk.as_str()).collect();
    let full_bytes: usize = chunks.iter().map(|c| c.len()).sum();
    let total_lines: usize = full_bytes / line.len().max(1);

    let mut capture_samples = Vec::with_capacity(samples);
    let mut head = String::new();
    let mut head_full = false;
    for _ in 0..samples {
        head.clear();
        head_full = false;
        let start = Instant::now();
        for text in &chunks {
            ingest((&mut head, &mut head_full), text);
        }
        capture_samples.push(micros(start.elapsed()));
    }
    spread("capture (8 MiB, head retention)", &capture_samples);

    // The retained windows the formatter actually sees.
    let tail_owned: String = chunks[chunks.len() - 16..].concat();
    let tail_start = {
        let mut start = tail_owned.len().saturating_sub(MAX_TAIL_BYTES);
        while start < tail_owned.len() && !tail_owned.is_char_boundary(start) {
            start += 1;
        }
        start
    };
    let tail_src = &tail_owned[tail_start..];
    let head_src = head.as_str();
    let totals = OutputTotals::new(total_lines, full_bytes);
    let mut window_samples = Vec::with_capacity(samples);
    let mut rendered = 0usize;
    for _ in 0..samples {
        let start = Instant::now();
        let window = build_window(
            head_src,
            tail_src,
            totals,
            TruncationOptions::default(),
            DEFAULT_HEAD_SHARE,
            Some("/tmp/bash-1234.log"),
        );
        rendered = window.as_ref().map(|w| w.content.len()).unwrap_or(0);
        window_samples.push(micros(start.elapsed()));
    }
    spread("window (build + join)", &window_samples);

    // What the caller would have received before the change: the tail plus the
    // notice text the old formatter appended.
    let tail_only = tools::truncate::truncate_tail(tail_src, TruncationOptions::default());
    let before = tail_only.content.len()
        + format!(
            "\n\n[Showing lines {}-{} of {}. Full output: /tmp/bash-1234.log]",
            total_lines - tail_only.output_lines + 1,
            total_lines,
            total_lines
        )
        .len();
    let pct = if before == 0 {
        0.0
    } else {
        (rendered as f64 / before as f64 - 1.0) * 100.0
    };
    println!(
        "context: {total_lines} lines / {} MiB total",
        full_bytes / (1024 * 1024)
    );
    println!("  tail-only {before} B -> window {rendered} B ({pct:+.1}%)");
}
