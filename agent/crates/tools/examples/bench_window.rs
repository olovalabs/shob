//! Head+tail window cost: cargo run -p tools --example bench_window -- 200
//!
//! Reports raw microsecond samples; it asserts nothing about wall-clock
//! performance. Run it on the same machine before and after a change.
//!
//! It measures the actual capture pipeline and the final failure window, with
//! no native shell, network, or inference:
//!
//! 1. `capture` — sanitization, bounded head/tail retention, progress snapshot,
//!    and complete-log persistence through the in-memory execution backend.
//! 2. `window` — building the head+tail window once, after the command failed.
//!
//! Both are reported as per-sample microseconds plus a best/worst spread so a
//! regression is visible rather than asserted.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tools::output_window::{build_window, OutputTotals, DEFAULT_HEAD_SHARE};
use tools::shell_output::{execute_shell_with_capture, ShellCaptureOptions};
use tools::truncate::TruncationOptions;
use tools::{ExecutionEnv, FileSystem, InMemoryExecutionEnv, ShellScript};

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

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let samples = std::env::args()
        .nth(1)
        .map(|n| n.parse::<usize>().unwrap())
        .unwrap_or(200)
        .clamp(1, 1_000);
    // 8 MiB of realistic log lines, streamed by the actual capture consumer.
    let line = "error[E0308]: mismatched types --> src/main.rs:12:5 expected `u32`, found `&str`\n";
    let chunk = line.repeat(8192 / line.len());
    let full = chunk.repeat(1024);
    let full_bytes = full.len();
    let total_lines = full_bytes / line.len();
    let memory = Arc::new(InMemoryExecutionEnv::new());
    memory
        .register_shell("log", ShellScript::success(full))
        .await;
    let env: Arc<dyn ExecutionEnv> = memory.clone();

    let mut capture_samples = Vec::with_capacity(samples);
    let capture = loop {
        let start = Instant::now();
        let capture = execute_shell_with_capture(&env, "log", ShellCaptureOptions::default())
            .await
            .unwrap();
        capture_samples.push(micros(start.elapsed()));
        if let Some(path) = &capture.full_output_path {
            memory
                .remove(&path.to_string_lossy(), false, false, None)
                .await
                .unwrap();
        }
        if capture_samples.len() == samples {
            break capture;
        }
    };
    spread("capture (8 MiB, in-memory spool)", &capture_samples);

    // The retained windows the formatter actually sees, not mirrored state.
    let tail_src = capture.output.as_str();
    let head_src = capture.head_output.as_str();
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
