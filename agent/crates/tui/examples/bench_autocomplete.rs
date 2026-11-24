//! Micro-benchmark: cost of path autocomplete on the TUI thread.
//! Run with: cargo run --release -p tui --example bench_autocomplete -- [entries] [samples]
//!
//! Builds a scratch directory with `entries` files, then measures
//! `get_suggestions` the way the TUI calls it: once per keystroke while the
//! user types a path prefix, and once per caret move through an existing
//! token. Asserts nothing about wall-clock performance; run on the same
//! machine before and after a change.

use std::time::Instant;

use tui::{AutocompleteProvider, FilePathAutocompleteProvider};

fn percentile(samples: &mut [f64], p: f64) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let index = (((samples.len() - 1) as f64) * p).round() as usize;
    samples[index]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let entries: usize = args
        .next()
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(2_049);
    let samples: usize = args.next().and_then(|arg| arg.parse().ok()).unwrap_or(200);

    let dir = std::env::temp_dir().join(format!("shob-bench-autocomplete-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    // A mix of names that share a prefix so the filter does real work.
    for i in 0..entries {
        let name = if i % 3 == 0 {
            format!("src{i:05}.rs")
        } else if i % 3 == 1 {
            format!("tests{i:05}_helper.rs")
        } else {
            format!("README{i:05}.md")
        };
        std::fs::write(dir.join(name), "")?;
    }
    let provider = FilePathAutocompleteProvider::with_root(dir.clone());

    // Keystroke timeline for `src…`: prefixes grow one char at a time, which is
    // how the provider is actually driven while typing.
    let keystrokes = ["s", "sr", "src", "src0", "src00"];
    let mut per_keystroke: Vec<Vec<f64>> = vec![Vec::new(); keystrokes.len()];
    // Counts how often a prefix that objectively matches hundreds of files came
    // back EMPTY — the baseline caps READ entries (50) not matches, so
    // suggestions silently disappear when readdir order puts no match in the
    // first 50 entries. A fix must drive these to zero.
    let mut empty_results = vec![0usize; keystrokes.len()];
    for _ in 0..samples {
        for (step, prefix) in keystrokes.iter().enumerate() {
            let input = format!("@{prefix}");
            let start = Instant::now();
            let suggestions = provider.get_suggestions(&input, input.len());
            let elapsed = start.elapsed().as_secs_f64() * 1000.0;
            match suggestions {
                Some(s) if !s.items.is_empty() => {}
                _ => empty_results[step] += 1,
            }
            per_keystroke[step].push(elapsed);
        }
    }
    // Caret moves through an already-typed token: the gate passes these to the
    // provider unchanged, so every caret move pays the same read_dir cost.
    let mut caret_moves: Vec<f64> = Vec::new();
    let input = "@src00000".to_string();
    for _ in 0..samples {
        for cursor in [1, 3, input.len()] {
            let start = Instant::now();
            let _ = provider.get_suggestions(&input, cursor);
            caret_moves.push(start.elapsed().as_secs_f64() * 1000.0);
        }
    }
    // A prefix that matches nothing still scans the whole directory today.
    let mut no_match: Vec<f64> = Vec::new();
    let input = "@zzzzz";
    for _ in 0..samples {
        let start = Instant::now();
        let suggestions = provider.get_suggestions(input, input.len());
        no_match.push(start.elapsed().as_secs_f64() * 1000.0);
        assert!(suggestions.is_none());
    }

    println!("path autocomplete, {entries} entries, {samples} samples (all on the TUI thread)");
    for (step, prefix) in keystrokes.iter().enumerate() {
        let times = &mut per_keystroke[step];
        let empty = empty_results[step];
        println!(
            "  type {prefix:<8} p50={:7.3} ms  p95={:7.3} ms  empty={empty}/{samples}",
            percentile(times, 0.50),
            percentile(times, 0.95),
        );
    }
    println!(
        "  caret move p50={:7.3} ms  p95={:7.3} ms",
        percentile(&mut caret_moves, 0.50),
        percentile(&mut caret_moves, 0.95),
    );
    println!(
        "  no match   p50={:7.3} ms  p95={:7.3} ms",
        percentile(&mut no_match, 0.50),
        percentile(&mut no_match, 0.95),
    );

    std::fs::remove_dir_all(&dir)?;
    Ok(())
}
