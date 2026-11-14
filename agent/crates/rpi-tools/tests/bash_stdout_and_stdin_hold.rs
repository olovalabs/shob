//! The two ways a shell command can keep the tool waiting *after the command
//! itself is done* — the everyday "`npm run` 卡住" report.
//!
//! Neither is a property of the shell; both are properties of how the child is
//! wired, and both are invisible in a short command:
//!
//! 1. **A grandchild holding the inherited pipes.** A script that starts a
//!    watcher, a dev-server, or a background daemon leaves a process running that
//!    inherited stdout and stderr. Reading those to EOF then waits for a process
//!    the command no longer owns — and no timeout rescues it, because the timeout
//!    wraps the *child* wait and the child already exited successfully.
//! 2. **An inherited stdin that is never closed.** Anything that prompts — `npx`
//!    asking to install a package, `npm init`, `read` in a script, `git` asking
//!    for credentials — blocks on a pipe nothing will ever write to or close. It
//!    does not fail fast on EOF; it hangs until the timeout.
//!
//! These tests need a real shell, so they use `OsExecutionEnv` and are skipped
//! (not failed) where no bash exists.

use std::time::{Duration, Instant};

use rpi_tools::{ExecutionErrorCode, OsExecutionEnv, Shell, ShellExecOptions};

fn scratch(tag: &str) -> std::path::PathBuf {
    let tmp = std::env::temp_dir().join(format!("pi-tools-{tag}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp);
    tmp
}

/// A `sleep` that outlives the command is the minimal shape of `npm run dev &`:
/// the command finished and printed its output, but the inherited pipe stays
/// open, so a reader waiting for EOF waits for the wrong process.
#[tokio::test]
async fn a_grandchild_holding_stdout_does_not_hold_the_call_open() {
    let tmp = scratch("pipe-hold");
    let env = OsExecutionEnv::with_cwd(tmp.clone());
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        env.exec("sleep 5 & echo started", ShellExecOptions::default()),
    )
    .await;
    let elapsed = started.elapsed();
    let _ = std::fs::remove_dir_all(&tmp);

    let output = match result {
        Ok(Ok(output)) => output,
        Ok(Err(error)) if error.code == ExecutionErrorCode::ShellUnavailable => return,
        Ok(Err(error)) => panic!("exec failed: {error:?}"),
        Err(_) => panic!("exec never returned: a background child held the stdout pipe"),
    };
    assert!(
        output.stdout.contains("started"),
        "the command's own output must still be captured: {:?}",
        output.stdout
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "waited {elapsed:?} on a process the command did not own (the grandchild's pipe held it open)"
    );
}

/// Already-read partial characters must be finalized even when a descendant
/// keeps both pipes open past the bounded drain grace period.
#[cfg(unix)]
#[tokio::test]
async fn bounded_pipe_drain_flushes_bytes_already_read() {
    let tmp = tempfile::tempdir().unwrap();
    let env = OsExecutionEnv::with_cwd(tmp.path().to_path_buf());
    let normal = match env
        .exec(
            "printf '\\344\\270'; sleep 0.05",
            ShellExecOptions::default(),
        )
        .await
    {
        Ok(output) => output,
        Err(error) if error.code == ExecutionErrorCode::ShellUnavailable => return,
        Err(error) => panic!("exec failed: {error:?}"),
    };
    assert!(!normal.stdout.is_empty());

    let mut stdout_callback = String::new();
    let mut stderr_callback = String::new();
    let started = Instant::now();
    let bounded = tokio::time::timeout(
        Duration::from_secs(1),
        env.exec(
            "printf '\\344\\270'; printf '\\344\\270' >&2; sleep 0.05; sleep 2 &",
            ShellExecOptions {
                on_stdout: Some(Box::new(|chunk| stdout_callback.push_str(chunk))),
                on_stderr: Some(Box::new(|chunk| stderr_callback.push_str(chunk))),
                ..Default::default()
            },
        ),
    )
    .await
    .expect("bounded drain waited for descendant EOF")
    .expect("exec failed");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(bounded.stdout, normal.stdout);
    assert_eq!(bounded.stderr, normal.stdout);
    assert_eq!(stdout_callback, bounded.stdout);
    assert_eq!(stderr_callback, bounded.stderr);
}

/// `cat` with no arguments reads stdin and exits at EOF. If stdin is left as an
/// open pipe nothing will ever close, it blocks instead.
#[tokio::test]
async fn a_command_reading_stdin_gets_eof_instead_of_waiting() {
    let tmp = scratch("stdin-eof");
    let env = OsExecutionEnv::with_cwd(tmp.clone());
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        env.exec(
            "cat; echo after-cat",
            ShellExecOptions {
                timeout: Some(10.0),
                ..Default::default()
            },
        ),
    )
    .await;
    let elapsed = started.elapsed();
    let _ = std::fs::remove_dir_all(&tmp);

    let output = match result {
        Ok(Ok(output)) => output,
        Ok(Err(error)) if error.code == ExecutionErrorCode::ShellUnavailable => return,
        // Blocking until the timeout is the failure this test exists for: the
        // command never saw EOF, so it never got past `cat`.
        Ok(Err(error)) => panic!("waited on an open stdin, then: {error:?}"),
        Err(_) => panic!("exec never returned: stdin was left open"),
    };
    assert!(
        output.stdout.contains("after-cat"),
        "stdin reached EOF, so the rest of the command ran: {:?}",
        output.stdout
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "waited {elapsed:?} on a stdin that should have been closed"
    );
}

/// Guard for the path above: closing stdin early must not defeat the timeout, and
/// the timeout must still take the whole tree with it.
#[tokio::test]
async fn a_command_that_ignores_the_timeout_is_still_killed() {
    let tmp = scratch("timeout-kill");
    let env = OsExecutionEnv::with_cwd(tmp.clone());
    let started = Instant::now();
    let result = env
        .exec(
            "sleep 30",
            ShellExecOptions {
                timeout: Some(1.0),
                ..Default::default()
            },
        )
        .await;
    let elapsed = started.elapsed();
    let _ = std::fs::remove_dir_all(&tmp);

    match result {
        Err(error) if error.code == ExecutionErrorCode::ShellUnavailable => return,
        Err(error) => assert_eq!(error.code, ExecutionErrorCode::Timeout, "{error:?}"),
        Ok(output) => panic!("a hung command completed: {output:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(5),
        "the timeout took {elapsed:?} to fire"
    );
}

/// Output larger than the pipe buffer (64 KiB on Linux) must be read while the
/// command runs. Waiting for the child before reading deadlocks: the command
/// blocks writing to a full pipe, and the reader waits for it to exit. A broad
/// `rg` over a docs folder is enough to hit it.
#[tokio::test]
async fn output_larger_than_the_pipe_buffer_is_read_while_the_command_runs() {
    let tmp = scratch("pipe-full");
    let env = OsExecutionEnv::with_cwd(tmp.clone());
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        env.exec(
            "(head -c 300000 /dev/zero | tr '\\0' a; echo) & (head -c 300000 /dev/zero | tr '\\0' b >&2) & wait",
            ShellExecOptions::default(),
        ),
    )
    .await;
    let elapsed = started.elapsed();
    let _ = std::fs::remove_dir_all(&tmp);

    let output = match result {
        Ok(Ok(output)) => output,
        Ok(Err(error)) if error.code == ExecutionErrorCode::ShellUnavailable => return,
        Ok(Err(error)) => panic!("exec failed: {error:?}"),
        Err(_) => panic!("exec never returned: nothing drained the full stdout pipe"),
    };
    assert_eq!(output.stdout.trim_end().len(), 300_000);
    assert_eq!(output.stderr.len(), 300_000);
    assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
}

/// Dropping the execution future must kill the entire process group, not just
/// stop polling it. A background write makes surviving descendants observable.
#[cfg(unix)]
#[tokio::test]
async fn dropped_execution_stops_background_work() {
    let dir = tempfile::tempdir().unwrap();
    let env = OsExecutionEnv::with_cwd(dir.path().to_path_buf());
    let (ready, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut execution = Box::pin(env.exec(
        "(sleep 0.3; printf leaked > escaped) & printf ready; wait",
        ShellExecOptions {
            on_stdout: Some(Box::new(move |_| {
                let _ = ready.send(());
            })),
            ..Default::default()
        },
    ));
    tokio::select! {
        _ = receiver.recv() => {}
        result = &mut execution => panic!("execution ended before drop: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(2)) => panic!("shell never started"),
    }
    drop(execution);
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert!(
        !dir.path().join("escaped").exists(),
        "dropped execution left its descendant alive"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn timeout_and_cancellation_interrupt_full_output_backpressure() {
    for cancel_execution in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let env = OsExecutionEnv::with_cwd(dir.path().to_path_buf());
        let token = tokio_util::sync::CancellationToken::new();
        // Deliberately do not consume output: cancellation must still be driven.
        let (output, _receiver) = tokio::sync::mpsc::channel(1);
        let (ready, mut ready_receiver) = tokio::sync::mpsc::unbounded_channel();
        let execution = env.exec(
            "(sleep 0.3; printf leaked > escaped) & printf ready; head -c 1000000 /dev/zero; wait",
            ShellExecOptions {
                timeout: Some(0.1),
                cancel: Some(&token),
                output: Some(output),
                capture_output: false,
                on_stdout: Some(Box::new(move |_| {
                    let _ = ready.send(());
                })),
                ..Default::default()
            },
        );
        tokio::pin!(execution);
        tokio::select! {
            _ = ready_receiver.recv() => {}
            result = &mut execution => panic!("execution ended before output: {result:?}"),
        }
        if cancel_execution {
            token.cancel();
        }
        let error = tokio::time::timeout(Duration::from_secs(2), execution)
            .await
            .expect("backpressure prevented process termination")
            .expect_err("execution must stop");
        assert_eq!(
            error.code,
            if cancel_execution {
                ExecutionErrorCode::Aborted
            } else {
                ExecutionErrorCode::Timeout
            }
        );
        tokio::time::sleep(Duration::from_millis(450)).await;
        assert!(
            !dir.path().join("escaped").exists(),
            "termination left a background descendant alive"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn slow_output_consumer_preserves_buffered_bytes_after_child_exit() {
    let dir = tempfile::tempdir().unwrap();
    let env = OsExecutionEnv::with_cwd(dir.path().to_path_buf());
    let (output, mut receiver) = tokio::sync::mpsc::channel(1);
    let execution = env.exec(
        "head -c 100000 /dev/zero",
        ShellExecOptions {
            output: Some(output),
            capture_output: false,
            timeout: Some(5.0),
            ..Default::default()
        },
    );
    let consumer = async {
        let mut bytes = 0;
        while let Some(chunk) = receiver.recv().await {
            match chunk {
                rpi_tools::ShellOutputChunk::Stdout(text) => bytes += text.len(),
                rpi_tools::ShellOutputChunk::Stderr(text) => panic!("unexpected stderr: {text}"),
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        bytes
    };
    let (result, bytes) = tokio::time::timeout(Duration::from_secs(6), async {
        tokio::join!(execution, consumer)
    })
    .await
    .expect("output backpressure stalled completion");
    let result = result.expect("shell execution");
    assert_eq!(bytes, 100_000);
    assert!(result.stdout.is_empty() && result.stderr.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_after_shell_exit_still_stops_descendants() {
    let dir = tempfile::tempdir().unwrap();
    let env = OsExecutionEnv::with_cwd(dir.path().to_path_buf());
    let token = tokio_util::sync::CancellationToken::new();
    let (ready, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let execution = env.exec(
        "(sleep 0.3; printf leaked > escaped) & printf ready",
        ShellExecOptions {
            cancel: Some(&token),
            on_stdout: Some(Box::new(move |_| {
                let _ = ready.send(());
            })),
            ..Default::default()
        },
    );
    tokio::pin!(execution);
    tokio::select! {
        _ = receiver.recv() => {}
        result = &mut execution => panic!("execution ended before cancellation: {result:?}"),
    }
    token.cancel();
    let error = execution.await.expect_err("cancellation must be reported");
    assert_eq!(error.code, ExecutionErrorCode::Aborted);
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert!(
        !dir.path().join("escaped").exists(),
        "reaped shell left its descendant alive"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn continuously_writing_descendant_does_not_hold_completion_open() {
    let dir = tempfile::tempdir().unwrap();
    let env = OsExecutionEnv::with_cwd(dir.path().to_path_buf());
    let mut pid_text = String::new();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        env.exec(
            "yes x & printf '%s\\n' \"$!\" >&2",
            ShellExecOptions {
                capture_output: false,
                on_stderr: Some(Box::new(|text| pid_text.push_str(text))),
                ..Default::default()
            },
        ),
    )
    .await;
    // Successful execution deliberately does not kill background descendants.
    if let Ok(pid) = pid_text.trim().parse::<i32>() {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    let output = result
        .expect("continuously ready descendant output prevented completion")
        .expect("shell execution");
    assert_eq!(output.exit_code, 0);
    assert!(output.stdout.is_empty());
}
