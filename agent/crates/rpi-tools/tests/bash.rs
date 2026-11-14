//! Mirrors `packages/agent/test/harness/tools.test.ts` "bash" describe block —
//! non-zero exit → Err, "(no output)", truncation temp-file, oversized-final-line
//! size report, command prefix, and output combination. The TS runs against a
//! real `NodeExecutionEnv`+bash; the Rust port runs the SAME invariants against
//! `InMemoryExecutionEnv` with canned `ShellScript`s (deterministic, no-platform-
//! bash dependency) plus the `OsExecutionEnv` for a couple of real-shell smoke
//! checks gated on bash availability.
//!
//! See the initial port notes (retired) for the divergence note on the timeout-truncated
//! output case (the TS `TimeoutOutputExecutionEnv` cannot be expressed against
//! `InMemoryExecutionEnv` without a blocking-override env like `abort_bracketing`).

use std::sync::Arc;
use std::time::Duration;

use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_tools::{
    BashToolOptions, ExecutionToolContext, FileSystem, InMemoryExecutionEnv, ShellScript,
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES,
};
use tokio_util::sync::CancellationToken;

fn fresh_context() -> (Arc<InMemoryExecutionEnv>, ExecutionToolContext) {
    let env = Arc::new(InMemoryExecutionEnv::with_cwd("/tmp/work".into()));
    let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
    let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
    let ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));
    (env, ctx)
}

async fn run_bash(
    tool: Arc<dyn AgentTool>,
    params: serde_json::Value,
) -> Result<rpi_agent::types::AgentToolResult, AgentError> {
    let signal = CancellationToken::new();
    let on_update = Arc::new(|_p| ());
    let prepared = tool
        .prepare_arguments(params.clone())
        .unwrap_or_else(|_| params);
    tool.execute("bash-x", prepared, signal, on_update).await
}

fn text_output(r: &rpi_agent::types::AgentToolResult) -> String {
    use rpi_agent::types::TextContentOrImage;
    r.content
        .iter()
        .filter_map(|c| match c {
            TextContentOrImage::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn err_text(e: &AgentError) -> String {
    match e {
        AgentError::Tool(m) => m.clone(),
        // The bash tool now reports failed commands as StructuredTool so the
        // tool result can carry exit_code / failure_kind. The message lives in
        // content, not in the error's Display.
        AgentError::StructuredTool(r) => text_output(r),
        other => format!("{other:?}"),
    }
}

#[tokio::test]
async fn no_output_renders_placeholder() {
    let (env, ctx) = fresh_context();
    env.register_shell("noop", ShellScript::success("")).await;
    let tool = rpi_tools::create_bash_tool(&ctx, None);
    let r = run_bash(tool, serde_json::json!({ "command": "noop" }))
        .await
        .expect("ok");
    assert_eq!(text_output(&r), "(no output)");
}

#[tokio::test]
async fn combines_stdout_and_stderr() {
    let (env, ctx) = fresh_context();
    // ShellScript carries separate stdout/stderr but the capture layer merges
    // them into one rolling buffer. Seed a script with both non-empty.
    env.register_shell(
        "both",
        ShellScript {
            stdout: "out".into(),
            stderr: "err".into(),
            exit_code: 0,
            delay_ms: None,
        },
    )
    .await;
    let tool = rpi_tools::create_bash_tool(&ctx, None);
    let r = run_bash(tool, serde_json::json!({ "command": "both" }))
        .await
        .expect("ok");
    let out = text_output(&r);
    assert!(out.contains("out"), "{out}");
    assert!(out.contains("err"), "{out}");
}

#[tokio::test]
async fn nonzero_exit_returns_error_with_output() {
    let (env, ctx) = fresh_context();
    env.register_shell(
        "fail",
        ShellScript {
            stdout: "failed".into(),
            stderr: String::new(),
            exit_code: 7,
            delay_ms: None,
        },
    )
    .await;
    let tool = rpi_tools::create_bash_tool(&ctx, None);
    let err = run_bash(tool, serde_json::json!({ "command": "fail" }))
        .await
        .expect_err("nonzero exit → Err");
    let msg = err_text(&err);
    assert!(msg.contains("failed"), "{msg}");
    assert!(msg.contains("Command exited with code 7"), "{msg}");
    // PR-B (fork issue #14): the failure stays isError=true in the agent loop,
    // but details now says this was a command result, not a broken tool.
    let AgentError::StructuredTool(result) = &err else {
        panic!("bash failures must carry structured details: {err:?}");
    };
    assert_eq!(result.details["exit_code"], 7);
    assert_eq!(result.details["failure_kind"], "command_failed");
}

#[tokio::test]
async fn abort_returns_error() {
    let (env, ctx) = fresh_context();
    env.register_shell(
        "slow",
        ShellScript {
            stdout: "x".into(),
            stderr: String::new(),
            exit_code: 0,
            delay_ms: Some(1000),
        },
    )
    .await;
    let tool = rpi_tools::create_bash_tool(&ctx, None);
    let signal = CancellationToken::new();
    let on_update = Arc::new(|_p| ());
    let handle = {
        let tool = tool.clone();
        let sig = signal.clone();
        tokio::spawn(async move {
            tool.execute(
                "bash-abort",
                serde_json::json!({ "command": "slow" }),
                sig,
                on_update,
            )
            .await
        })
    };
    // Let the command enter its delay, then abort.
    tokio::time::sleep(Duration::from_millis(20)).await;
    signal.cancel();
    let err = handle.await.expect("join").expect_err("abort → Err");
    let msg = err_text(&err);
    assert!(msg.contains("aborted") || msg.contains("Aborted"), "{msg}");
}

#[tokio::test]
async fn persists_truncated_full_output_to_temp_file() {
    let (env, ctx) = fresh_context();
    // Exceed the line cap: the complete log must be available after capture.
    let lines: String = (0..(DEFAULT_MAX_LINES + 1))
        .map(|i| format!("line-{}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    env.register_shell("big", ShellScript::success(lines)).await;

    let tool = rpi_tools::create_bash_tool(&ctx, None);
    let r = run_bash(tool, serde_json::json!({ "command": "big" }))
        .await
        .expect("ok");
    // Truncation metadata + a fullOutputPath.
    let trunc = r.details.get("truncation").expect("truncation present");
    assert_eq!(trunc["truncated"], true);
    assert_eq!(trunc["truncated_by"], "lines");
    assert_eq!(trunc["total_lines"], DEFAULT_MAX_LINES + 1);
    assert_eq!(trunc["output_lines"], DEFAULT_MAX_LINES);
    let full_path = r
        .details
        .get("full_output_path")
        .and_then(|v| v.as_str())
        .expect("full_output_path present");
    let full = env
        .read_text_file(full_path, None)
        .await
        .expect("read full output");
    assert!(full.contains("line-1\nline-2"), "{full}");
    assert!(
        full.contains(&format!(
            "line-{}\nline-{}",
            DEFAULT_MAX_LINES,
            DEFAULT_MAX_LINES + 1
        )),
        "{full}"
    );
}

#[tokio::test]
async fn complete_log_outlives_the_bounded_display_tail() {
    let (env, _) = fresh_context();
    let expected = format!("BEGIN\n{}END\n", "界abcdefghij\n".repeat(150_000));
    env.register_shell("large-log", ShellScript::success(expected.clone()))
        .await;
    let execution_env: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
    let capture = rpi_tools::shell_output::execute_shell_with_capture(
        &execution_env,
        "large-log",
        rpi_tools::shell_output::ShellCaptureOptions::default(),
    )
    .await
    .expect("capture");
    assert!(capture.truncated);
    assert!(capture.output.len() <= DEFAULT_MAX_BYTES);
    assert_eq!(capture.truncation.total_bytes, expected.len());
    let path = capture.full_output_path.expect("complete log path");
    let full = env
        .read_text_file(&path.to_string_lossy(), None)
        .await
        .unwrap();
    assert_eq!(full.len(), expected.len());
    assert!(
        full == expected,
        "saved log must contain the prefix, middle and tail"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sanitizes_ansi_sequences_split_across_pipe_reads() {
    let dir = tempfile::tempdir().unwrap();
    let env: Arc<dyn rpi_tools::ExecutionEnv> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        dir.path().to_path_buf(),
    ));
    let capture = rpi_tools::shell_output::execute_shell_with_capture(
        &env,
        "printf '\\033['; printf 'visible\\n' >&2; sleep 0.02; printf '31mred\\033]title'; sleep 0.02; printf '\\007plain\\033[0m'",
        rpi_tools::shell_output::ShellCaptureOptions::default(),
    ).await.expect("capture");
    assert!(capture.output.contains("visible\n"));
    assert!(capture.output.contains("redplain"));
    assert!(!capture.output.contains("31m"));
    assert!(!capture.output.contains("title"));
    assert!(!capture.output.contains('\x1b'));
}

#[cfg(unix)]
#[tokio::test]
async fn native_capture_persists_more_than_its_rolling_tail() {
    let dir = tempfile::tempdir().unwrap();
    let env: Arc<dyn rpi_tools::ExecutionEnv> = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(
        dir.path().to_path_buf(),
    ));
    let capture = rpi_tools::shell_output::execute_shell_with_capture(
        &env,
        "printf BEGIN; head -c 2000000 /dev/zero | tr '\\0' a; printf END",
        rpi_tools::shell_output::ShellCaptureOptions::default(),
    )
    .await
    .expect("native capture");
    assert!(capture.output.len() <= DEFAULT_MAX_BYTES);
    assert!(capture.output.ends_with("END"));
    let path = capture.full_output_path.expect("complete native log");
    let bytes = tokio::fs::read(&path).await.unwrap();
    tokio::fs::remove_file(path).await.unwrap();
    assert_eq!(bytes.len(), 2_000_008);
    assert!(bytes.starts_with(b"BEGIN") && bytes.ends_with(b"END"));
    assert!(bytes[5..bytes.len() - 3].iter().all(|byte| *byte == b'a'));
}

#[tokio::test]
async fn reports_oversized_final_line_size() {
    let (env, ctx) = fresh_context();
    // One line that exceeds the 50KB byte cap → last_line_partial path.
    let big_line: String = "0".repeat(DEFAULT_MAX_BYTES + 5000);
    env.register_shell("oneline", ShellScript::success(big_line))
        .await;

    let tool = rpi_tools::create_bash_tool(&ctx, None);
    let r = run_bash(tool, serde_json::json!({ "command": "oneline" }))
        .await
        .expect("ok");
    let out = text_output(&r);
    // The TS expects "Showing last <size> of line 1 (line is <size>). Full output:".
    assert!(out.contains("Showing last"), "{out}");
    assert!(out.contains("line 1"), "{out}");
}

#[tokio::test]
async fn supports_command_prefix() {
    let (env, ctx) = fresh_context();
    // Prefix sets a variable; the command echoes it. The InMemory shell matches
    // by longest prefix, so register a script for the FULL composed command the
    // tool emits (prefix + "\n" + command).
    env.register_shell("value=hello\necho hello", ShellScript::success("hello"))
        .await;
    let tool = rpi_tools::create_bash_tool(
        &ctx,
        Some(BashToolOptions {
            command_prefix: Some("value=hello".into()),
            default_timeout: None,
            prepare: None,
        }),
    );
    // The shell_script prefix-matches on the composed command start; echo is the
    // user command. Register a broader prefix to be safe.
    env.register_shell("value=hello", ShellScript::success("hello"))
        .await;
    let r = run_bash(tool, serde_json::json!({ "command": "echo ignored" }))
        .await
        .expect("ok");
    assert_eq!(text_output(&r), "hello");
}

#[test]
fn schema_describes_effective_default_timeout() {
    let (_, ctx) = fresh_context();
    for (options, expected) in [
        (None, 120.0),
        (Some(BashToolOptions::default()), 120.0),
        (
            Some(BashToolOptions {
                default_timeout: Some(30.0),
                ..Default::default()
            }),
            30.0,
        ),
    ] {
        let tool = rpi_tools::create_bash_tool(&ctx, options);
        assert!(tool
            .schema()
            .description
            .contains(&format!("{expected}-second timeout")));
        let params = serde_json::to_value(&tool.schema().parameters).unwrap();
        assert!(params["properties"]["timeout"]["description"]
            .as_str()
            .unwrap()
            .contains(&format!("defaults to {expected} seconds")));
    }
}

#[tokio::test]
async fn real_shell_honors_fallback_and_explicit_timeouts() {
    use rpi_tools::{ExecutionErrorCode, OsExecutionEnv, Shell, ShellExecOptions};

    let tmp = tempfile::tempdir().unwrap();
    let env = Arc::new(OsExecutionEnv::with_cwd(tmp.path().to_path_buf()));
    match env
        .exec(
            "echo ready",
            ShellExecOptions {
                timeout: Some(5.0),
                ..Default::default()
            },
        )
        .await
    {
        Err(error) if error.code == ExecutionErrorCode::ShellUnavailable => {
            eprintln!("skipping real-shell timeout test: {error}");
            return;
        }
        result => {
            result.expect("shell is available");
        }
    }
    let ctx = ExecutionToolContext::new(env, None);
    for (default_timeout, params) in [
        (0.1, serde_json::json!({ "command": "sleep 30" })),
        (
            0.1,
            serde_json::json!({ "command": "sleep 30", "timeout": null }),
        ),
        (
            30.0,
            serde_json::json!({ "command": "sleep 30", "timeout": 0.1 }),
        ),
    ] {
        let tool = rpi_tools::create_bash_tool(
            &ctx,
            Some(BashToolOptions {
                default_timeout: Some(default_timeout),
                ..Default::default()
            }),
        );
        let result = tokio::time::timeout(Duration::from_secs(10), run_bash(tool, params))
            .await
            .expect("bash must return within its deadline");
        let msg = err_text(&result.expect_err("slow command must time out"));
        assert!(msg.contains("Command timed out after 0.1s"), "{msg}");
    }
}

/// Fork issue #14 PR-B, compatibility version: keep `isError` semantics but
/// classify the failure in `details`. Pins the whole "how did the call end"
/// axis: command failed / cancelled / timed out / shell transport failed, plus
/// the success cell (no failure_kind).
#[tokio::test]
async fn review_issue14_failure_kind_axis_and_success_have_no_classification() {
    let (env, ctx) = fresh_context();
    env.register_shell(
        "fail",
        ShellScript {
            stdout: "grep: no match".into(),
            stderr: String::new(),
            exit_code: 1,
            delay_ms: None,
        },
    )
    .await;
    env.register_shell("ok", ShellScript::success("done")).await;
    let tool = rpi_tools::create_bash_tool(&ctx, None);

    let ok = run_bash(tool.clone(), serde_json::json!({ "command": "ok" }))
        .await
        .expect("success");
    assert!(ok.details.get("failure_kind").is_none(), "{:?}", ok.details);
    assert_eq!(ok.details["exit_code"], 0);

    let err = run_bash(tool.clone(), serde_json::json!({ "command": "fail" }))
        .await
        .expect_err("nonzero exit stays an error");
    let AgentError::StructuredTool(result) = &err else {
        panic!("expected StructuredTool: {err:?}");
    };
    assert_eq!(result.details["exit_code"], 1);
    assert_eq!(result.details["failure_kind"], "command_failed");
    // A probe command's output must still be visible: the classification is
    // additive, it must not swallow what the command said.
    assert!(text_output(result).contains("grep: no match"));

    // Timeout cell: InMemoryExecutionEnv does not model a deadline, so use the
    // real shell (same setup as `real_shell_honors_fallback_and_explicit_timeouts`).
    use rpi_tools::{ExecutionErrorCode, OsExecutionEnv, Shell, ShellExecOptions};
    let tmp = tempfile::tempdir().unwrap();
    let os_env = Arc::new(OsExecutionEnv::with_cwd(tmp.path().to_path_buf()));
    match os_env
        .exec(
            "echo ready",
            ShellExecOptions {
                timeout: Some(5.0),
                ..Default::default()
            },
        )
        .await
    {
        Err(error) if error.code == ExecutionErrorCode::ShellUnavailable => {
            eprintln!("skipping timeout cell: no bash on this host");
        }
        result => {
            result.expect("shell is available");
            let os_ctx =
                ExecutionToolContext::new(os_env.clone() as Arc<dyn rpi_tools::ExecutionEnv>, None);
            let slow_tool = rpi_tools::create_bash_tool(&os_ctx, None);
            let err = run_bash(
                slow_tool,
                serde_json::json!({ "command": "sleep 30", "timeout": 0.1 }),
            )
            .await
            .expect_err("timeout stays an error");
            let AgentError::StructuredTool(result) = &err else {
                panic!("expected StructuredTool: {err:?}");
            };
            assert_eq!(result.details["failure_kind"], "timeout");
        }
    }

    // Cancellation cell.
    let (env3, ctx3) = fresh_context();
    env3.register_shell(
        "slow3",
        ShellScript {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            delay_ms: Some(5000),
        },
    )
    .await;
    let cancel_tool = rpi_tools::create_bash_tool(&ctx3, None);
    let signal = CancellationToken::new();
    let on_update = Arc::new(|_p| ());
    let cancel = signal.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();
    });
    let err = cancel_tool
        .execute(
            "bash-x",
            serde_json::json!({ "command": "slow3" }),
            signal,
            on_update,
        )
        .await
        .expect_err("cancel stays an error");
    let AgentError::StructuredTool(result) = &err else {
        panic!("expected StructuredTool: {err:?}");
    };
    assert_eq!(result.details["failure_kind"], "cancelled");
}
