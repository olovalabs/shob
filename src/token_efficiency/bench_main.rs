//! `token-efficiency-bench`: run the replay benchmark over the recorded tasks.
//!
//! Usage:
//!
//! ```text
//! cargo run --bin token-efficiency-bench -- [--fixtures DIR] [--out FILE]
//! ```
//!
//! The benchmark does not call a provider. It replays the recorded tool results
//! in `bench/token-efficiency/fixtures/` through both pipelines and reports the
//! prompt tokens each would send.

use std::path::PathBuf;
use std::process::ExitCode;

use crate::token_efficiency::bench;
use crate::token_efficiency::config::TokenEfficiencySettings;

pub fn main() -> ExitCode {
    let mut fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("bench")
        .join("token-efficiency")
        .join("fixtures");
    let mut out: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fixtures" => match args.next() {
                Some(value) => fixtures = PathBuf::from(value),
                None => return usage("--fixtures needs a directory"),
            },
            "--out" => match args.next() {
                Some(value) => out = Some(PathBuf::from(value)),
                None => return usage("--out needs a file path"),
            },
            "--help" | "-h" => {
                println!("usage: token-efficiency-bench [--fixtures DIR] [--out FILE]");
                return ExitCode::SUCCESS;
            }
            other => return usage(&format!("unrecognised argument {other:?}")),
        }
    }

    let tasks = match bench::load_tasks(&fixtures) {
        Ok(tasks) if !tasks.is_empty() => tasks,
        Ok(_) => return usage(&format!("no task fixtures in {}", fixtures.display())),
        Err(error) => {
            eprintln!("could not read {}: {error}", fixtures.display());
            return ExitCode::FAILURE;
        }
    };

    let settings = TokenEfficiencySettings::default();
    let comparisons: Vec<_> = tasks
        .iter()
        .map(|task| bench::compare(task, &settings))
        .collect();

    let markdown = bench::report(&comparisons, &settings);
    print!("{markdown}");

    // Attribute the saving: disable exactly one feature and re-run.
    let ablations = bench::ablate(&tasks, &settings);
    let baseline_total: u64 = comparisons.iter().map(|c| c.baseline.prompt_tokens).sum();
    println!("\n{}", bench::ablation_report(&ablations, baseline_total));

    // Per-task detail makes the aggregate checkable.
    for comparison in &comparisons {
        println!(
            "\n{}: {} → {} tokens ({:.1}% fewer), {} read stubs, {} search stubs, {} minimized, {} anchor edits",
            comparison.task,
            comparison.baseline.prompt_tokens,
            comparison.optimized.prompt_tokens,
            comparison.prompt_reduction * 100.0,
            comparison.optimized.read_stubs,
            comparison.optimized.search_stubs,
            comparison.optimized.minimized,
            comparison.optimized.anchor_edits,
        );
    }

    if let Some(path) = out {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::write(&path, &markdown) {
            Ok(()) => println!("\nwrote {}", path.display()),
            Err(error) => {
                eprintln!("could not write {}: {error}", path.display());
                return ExitCode::FAILURE;
            }
        }
    }

    ExitCode::SUCCESS
}

fn usage(message: &str) -> ExitCode {
    eprintln!("{message}");
    eprintln!("usage: token-efficiency-bench [--fixtures DIR] [--out FILE]");
    ExitCode::FAILURE
}
