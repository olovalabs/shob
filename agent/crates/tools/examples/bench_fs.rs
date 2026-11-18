//! Run on both revisions with `cargo run --release -p tools --example bench_fs`.
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::time::Instant;

use serde_json::json;
use tokio_util::sync::CancellationToken;
use tools::{
    create_find_tool, create_grep_tool, create_ls_tool, create_read_tool, ExecutionToolContext,
    OsExecutionEnv,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let samples = std::env::args()
        .nth(1)
        .map(|arg| arg.parse::<usize>())
        .transpose()?
        .unwrap_or(25);
    if !(1..=200).contains(&samples) {
        return Err("samples must be between 1 and 200".into());
    }
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    let line = format!("{:<63}\n", "2026-11-18 INFO: benchmark fixture");
    let block = line.repeat(1024);
    let mut large = BufWriter::new(File::create(root.join("large.log"))?);
    for _ in 0..1024 {
        large.write_all(block.as_bytes())?;
    }
    large.flush()?;
    fs::write(root.join("small.log"), line.repeat(64))?;
    fs::create_dir(root.join("wide"))?;
    for n in 0..2048 {
        fs::write(root.join(format!("wide/file-{n:04}.txt")), "fixture\n")?;
    }
    fs::write(root.join("wide/special.rs"), "fixture needle\n")?;

    let env = Arc::new(OsExecutionEnv::with_cwd(root.to_owned()));
    let ctx = ExecutionToolContext::new(env, None);
    let cases = [
        (
            "read 20 lines / 4 KiB",
            create_read_tool(&ctx, None),
            json!({"path": "small.log", "limit": 20}),
        ),
        (
            "read 20 lines / 64 MiB",
            create_read_tool(&ctx, None),
            json!({"path": "large.log", "limit": 20}),
        ),
        (
            "ls / 2,049 entries",
            create_ls_tool(&ctx, None),
            json!({"path": "wide", "limit": 50}),
        ),
        (
            "find / 2,049 entries",
            create_find_tool(&ctx, None),
            json!({"path": "wide", "pattern": "*.txt", "limit": 50}),
        ),
        (
            "grep with glob / 2,049 entries",
            create_grep_tool(&ctx, None),
            json!({"path": "wide", "pattern": "needle", "glob": "*.rs", "mode": "files"}),
        ),
    ];
    for (label, tool, params) in cases {
        let mut timings = Vec::with_capacity(samples);
        for i in 0..=samples {
            let start = Instant::now();
            let result = tool
                .execute(
                    "bench",
                    params.clone(),
                    CancellationToken::new(),
                    Arc::new(|_| {}),
                )
                .await?;
            if i != 0 {
                timings.push(start.elapsed().as_secs_f64() * 1000.0);
            }
            std::hint::black_box(result);
        }
        timings.sort_by(f64::total_cmp);
        let p50 = timings[(samples * 50).div_ceil(100) - 1];
        let p95 = timings[(samples * 95).div_ceil(100) - 1];
        println!("{label:<32} p50={p50:8.3} ms  p95={p95:8.3} ms  n={samples}");
    }
    Ok(())
}
