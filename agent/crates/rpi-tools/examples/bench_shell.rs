//! Local shell latency benchmark: cargo run -p rpi-tools --example bench_shell -- 15
//! Samples include shell startup; no inference, network, approvals or UI rendering.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rpi_agent::types::TextContentOrImage;
use rpi_tools::{create_bash_tool, ExecutionToolContext, OsExecutionEnv, Shell, ShellExecOptions};
use tokio_util::sync::CancellationToken;

type Times = Arc<Mutex<Vec<(String, f64)>>>;
fn millis(time: Duration) -> f64 {
    time.as_secs_f64() * 1000.0
}
fn record(times: &Times, name: &str, time: Duration) {
    times.lock().unwrap().push((name.to_owned(), millis(time)));
}
fn text(result: &rpi_agent::types::AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|part| match part {
            TextContentOrImage::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let samples = std::env::args()
        .nth(1)
        .map(|n| n.parse::<usize>().unwrap())
        .unwrap_or(15)
        .clamp(1, 100);
    let times: Times = Arc::new(Mutex::new(Vec::new()));
    let dir = tempfile::tempdir().unwrap();
    let env = Arc::new(OsExecutionEnv::with_cwd(dir.path().into()));
    let tool = create_bash_tool(&ExecutionToolContext::new(env.clone(), None), None);
    for _ in 0..samples {
        // Control measurement: only Command::spawn(), not scheduling or execution.
        let mut command = tokio::process::Command::new("/bin/bash");
        command
            .args(["-c", ":"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let start = Instant::now();
        let mut child = command.spawn().unwrap();
        record(&times, "command_spawn", start.elapsed());
        assert!(child.wait().await.unwrap().success());

        let first = Arc::new(Mutex::new(None));
        let callback_first = first.clone();
        let first_error = Arc::new(Mutex::new(None));
        let callback_error = first_error.clone();
        let start = Instant::now();
        let out = env
            .exec(
                "printf ready; printf error >&2",
                ShellExecOptions {
                    on_stdout: Some(Box::new(move |chunk| {
                        if !chunk.is_empty() {
                            callback_first
                                .lock()
                                .unwrap()
                                .get_or_insert_with(|| start.elapsed());
                        }
                    })),
                    on_stderr: Some(Box::new(move |chunk| {
                        if !chunk.is_empty() {
                            callback_error
                                .lock()
                                .unwrap()
                                .get_or_insert_with(|| start.elapsed());
                        }
                    })),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(out.stdout, "ready");
        assert_eq!(out.stderr, "error");
        record(&times, "exec_first_stdout", first.lock().unwrap().unwrap());
        record(
            &times,
            "exec_first_stderr",
            first_error.lock().unwrap().unwrap(),
        );
        record(&times, "exec_total", start.elapsed());

        let explicit =
            OsExecutionEnv::with_cwd(dir.path().into()).with_shell_path("/bin/bash".into());
        let start = Instant::now();
        assert_eq!(
            explicit
                .exec("printf ready", Default::default())
                .await
                .unwrap()
                .stdout,
            "ready"
        );
        record(&times, "explicit_shell_total", start.elapsed());

        let updates = Arc::new(Mutex::new(Vec::<(Duration, String)>::new()));
        let callback_updates = updates.clone();
        let start = Instant::now();
        let result = tool.execute("paced", serde_json::json!({"command": "printf first; sleep 0.02; printf second; sleep 0.2"}), CancellationToken::new(), Arc::new(move |result| {
            let output = text(&result);
            if !output.is_empty() { callback_updates.lock().unwrap().push((start.elapsed(), output)); }
        })).await.unwrap();
        assert_eq!(text(&result), "firstsecond");
        let updates = updates.lock().unwrap();
        record(&times, "tool_first_output", updates.first().unwrap().0);
        record(
            &times,
            "tool_second_output",
            updates
                .iter()
                .find(|(_, output)| output.contains("second"))
                .unwrap()
                .0,
        );
        record(&times, "tool_paced_total", start.elapsed());
        drop(updates);

        let ready = Arc::new(tokio::sync::Notify::new());
        let callback_ready = ready.clone();
        let cancel = CancellationToken::new();
        let mut execution = Box::pin(env.exec(
            "printf ready; sleep 30",
            ShellExecOptions {
                cancel: Some(&cancel),
                on_stdout: Some(Box::new(move |_| callback_ready.notify_one())),
                ..Default::default()
            },
        ));
        tokio::select! {
            _ = ready.notified() => {},
            result = &mut execution => panic!("exited before cancellation: {result:?}"),
            _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("no ready output"),
        }
        let start = Instant::now();
        cancel.cancel();
        assert_eq!(
            execution.await.unwrap_err().code,
            rpi_tools::ExecutionErrorCode::Aborted
        );
        record(&times, "cancel_after_ready", start.elapsed());

        let start = Instant::now();
        assert_eq!(
            env.exec(
                "sleep 30",
                ShellExecOptions {
                    timeout: Some(0.02),
                    ..Default::default()
                }
            )
            .await
            .unwrap_err()
            .code,
            rpi_tools::ExecutionErrorCode::Timeout
        );
        record(&times, "timeout_20ms_total", start.elapsed());

        let start = Instant::now();
        let out = env
            .exec("head -c 1048576 /dev/zero | tr '\\0' x", Default::default())
            .await
            .unwrap();
        assert_eq!(out.stdout.len(), 1048576);
        record(&times, "exec_1mib", start.elapsed());

        let start = Instant::now();
        let result = tool
            .execute(
                "volume",
                serde_json::json!({"command": "head -c 1048576 /dev/zero | tr '\\0' x"}),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap();
        let path = result.details["full_output_path"]
            .as_str()
            .expect("truncated log path");
        let log_bytes = std::fs::metadata(path).unwrap().len();
        assert!(
            log_bytes >= 102400,
            "capture log missing even its bounded tail"
        );
        std::fs::remove_file(path).unwrap();
        record(&times, "tool_1mib", start.elapsed());
    }
    let times = times.lock().unwrap();
    let mut names: Vec<_> = times.iter().map(|(name, _)| name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    println!("metric,samples,p50_ms,p95_ms");
    for name in names {
        let mut values: Vec<_> = times
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| *value)
            .collect();
        values.sort_by(f64::total_cmp);
        println!(
            "{name},{},{:.3},{:.3}",
            values.len(),
            values[values.len() / 2],
            values[((values.len() * 95).div_ceil(100)).saturating_sub(1)]
        );
    }
}
