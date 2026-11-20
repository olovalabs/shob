//! Mirrors `packages/agent/src/harness/tools/bash.ts` — the built-in `bash`
//! `AgentTool`. Drives `execute_shell_with_capture` with an incremental
//! checkpoint throttle, and converts ALL failures (non-zero exit, timeout,
//! abort, executionError) into `Err(AgentError)` — the agent loop encodes those
//! into an error `ToolResultMessage`.
//!
//! The first chunk is emitted immediately. Later checkpoints are coalesced into
//! a short window (16 ms by default, overridable with
//! `SHOB_BASH_UPDATE_THROTTLE_MS`); a final flush always follows capture
//! resolution.

use futures::future::BoxFuture;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent::agent_tool::AgentTool;
use agent::error::AgentError;
use agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use ai::types::{ConstrainedSamplingConfig, ConstrainedStrictness, Tool};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::env::ExecutionEnv;
use crate::error::{ExecutionError, ExecutionErrorCode};
use crate::shell_compact::compact_output;
pub use crate::shell_compact::{ShellOutputMetrics, ShellOutputMode};
use crate::shell_output::{
    execute_shell_with_capture, ShellCaptureOptions, ShellCaptureProgress, ShellCaptureResult,
};
use crate::tools::tool_context::ExecutionToolContext;
use crate::truncate::{
    format_size, TruncationOptions, TruncationResult, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES,
};

/// Default throttle window for bash `on_update` flushes. The first chunk is
/// emitted immediately; this window only coalesces later checkpoints.
const BASH_UPDATE_THROTTLE_MS: u64 = 16;

/// The active checkpoint window, resolved once. Reading the environment on every
/// chunk allocated a `String` and ran a lookup in the read loop.
pub(super) fn update_throttle() -> Duration {
    static THROTTLE: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *THROTTLE.get_or_init(|| {
        std::env::var("SHOB_BASH_UPDATE_THROTTLE_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_millis(BASH_UPDATE_THROTTLE_MS))
    })
}

/// Default Bash deadline when neither the caller nor the host specifies one.
pub const DEFAULT_BASH_TIMEOUT_SECONDS: f64 = 120.0;

/// Input for the bash tool. Mirrors TS `BashToolInput`. `timeout` is in seconds.
#[derive(Debug, Clone, JsonSchema, Deserialize)]
pub struct BashInput {
    /// Bash command to execute.
    pub command: String,
    /// Timeout in seconds (optional; uses the host default, otherwise 120 seconds).
    #[serde(default)]
    pub timeout: Option<f64>,
    /// Output formatting: raw (default) preserves output; compact conservatively
    /// collapses recognized Cargo/pytest/Go/JS progress and saves the full log.
    #[serde(default)]
    pub output_mode: ShellOutputMode,
}

/// How a bash tool call ended. A non-zero exit is a *command result*, not a
/// tool malfunction: probes such as `grep` (no match) and `git config --get`
/// (missing key) legitimately return 1. Fork issue #14 §4 PR-B asks for this
/// distinction without changing `isError` yet, so the classification lives in
/// `details.failure_kind` and the agent loop keeps its existing error path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BashFailureKind {
    /// The command ran to completion but returned a non-zero exit code.
    CommandFailed,
    /// The user/tool cancelled the command.
    Cancelled,
    /// The command exceeded its timeout.
    Timeout,
    /// The shell or transport itself failed (spawn, I/O, etc.).
    ToolFailed,
}

/// Structured details for the bash tool. Mirrors TS `BashToolDetails`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BashToolDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    /// Raw exit code when the shell reported one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Why the call ended; absent on success. `CommandFailed` means "read the
    /// output before retrying", not "the tool is broken".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<BashFailureKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<ShellOutputMetrics>,
}

/// Mutable execution description passed to the optional prepare hook.
#[derive(Debug, Clone)]
pub struct BashExecution {
    pub command: String,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub inherit_env: bool,
}

/// Async hook called immediately before shell execution, matching native Pi's
/// `BashPrepare` extension point.
pub type BashPrepare =
    Arc<dyn Fn(&mut BashExecution) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// Options for `create_bash_tool`. Mirrors TS `BashToolOptions`.
#[derive(Clone, Default)]
pub struct BashToolOptions {
    /// Prepended to the command (separated by `\n`) before execution.
    pub command_prefix: Option<String>,
    /// Fallback timeout in seconds applied when the model doesn't pass one.
    /// `None` uses `DEFAULT_BASH_TIMEOUT_SECONDS`, not an unlimited deadline.
    /// A model-supplied timeout still wins.
    pub default_timeout: Option<f64>,
    /// Optional async hook that can adjust command, cwd, and environment.
    pub prepare: Option<BashPrepare>,
}

/// The built-in `bash` tool. Holds an `Arc<dyn ExecutionEnv>` (read-only view
/// suffices — bash does not touch the mutation queue).
pub struct BashTool {
    schema: Tool,
    env: Arc<dyn ExecutionEnv>,
    options: BashToolOptions,
}

impl BashTool {
    fn schema(default_timeout: f64) -> Tool {
        let mut params = serde_json::to_value(schemars::schema_for!(BashInput)).unwrap_or_default();
        params["properties"]["timeout"]["description"] =
            format!("Timeout in seconds (optional, defaults to {default_timeout} seconds).").into();
        Tool {
            name: "bash".to_string(),
            description: format!(
                "Execute a Bash command in the current working directory using Bash/POSIX syntax, even on Windows. PowerShell cmdlets (Get-ChildItem, Select-Object, Select-String) are not valid here; use ls, find, rg, and POSIX pipelines instead. Returns stdout and \
                 stderr. Output is truncated to last {DEFAULT_MAX_LINES} lines or {kb}KB \
                 (whichever is hit first). If truncated, full output is saved to a temp file. \
                 If the command failed and the output was truncated, the first lines are shown \
                 as well as the last, with the elided line count stated, so the cause is not \
                 hidden; nothing is summarised or dropped silently. \
                 output_mode defaults to raw; opt into compact to collapse recognized Cargo, pytest, Go, \
                 and JS passing/progress lines while retaining diagnostics and saving a complete sanitized log. \
                 Defaults to a {default_timeout}-second timeout; provide timeout in seconds to override it. \
                 A non-zero exit code is a command result: probe commands such as grep (no match) or \
                 git config --get (missing key) legitimately return 1. Read the output before deciding \
                 whether to retry; details carries exit_code and failure_kind.",
                kb = DEFAULT_MAX_BYTES / 1024
            ),
            parameters: ai::types::Schema::new(params),
            constrained_sampling: Some(ConstrainedSamplingConfig::JsonSchema {
                strict: ConstrainedStrictness::Prefer,
            }),
        }
    }
}

/// Build the `bash` tool from a context. Mirrors TS `createBashTool`.
pub fn create_bash_tool(
    context: &ExecutionToolContext,
    options: Option<BashToolOptions>,
) -> Arc<dyn AgentTool> {
    let options = options.unwrap_or_default();
    Arc::new(BashTool {
        schema: BashTool::schema(
            options
                .default_timeout
                .unwrap_or(DEFAULT_BASH_TIMEOUT_SECONDS),
        ),
        env: context.env().clone(),
        options,
    })
}

#[async_trait]
impl AgentTool for BashTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "bash"
    }
    fn execution_mode(&self) -> agent::types::ToolExecutionMode {
        agent::types::ToolExecutionMode::Sequential
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: BashInput = serde_json::from_value(params)
            .map_err(|e| AgentError::Validation(format!("bash input invalid: {e}")))?;
        let timeout = Some(resolve_timeout(
            input.timeout,
            self.options.default_timeout,
        )?);
        let mut execution = BashExecution {
            command: match &self.options.command_prefix {
                Some(p) => format!("{p}\n{}", input.command),
                None => input.command,
            },
            cwd: self.env.cwd().to_path_buf(),
            env: HashMap::new(),
            inherit_env: true,
        };
        if let Some(prepare) = &self.options.prepare {
            prepare(&mut execution).await.map_err(AgentError::Tool)?;
        }
        let command = execution.command;

        // Initial empty update (mirrors TS `onUpdate?.({ content: [], details: undefined })`).
        emit_update(&on_update, String::new(), None, None);

        // Capture owns one timer, so quiet chunks flush without another read.
        // Snapshot construction remains lazy and happens only on notification.
        let on_update_for_cb = on_update.clone();
        let on_chunk: Box<dyn FnMut(&str, &dyn Fn() -> Option<ShellCaptureProgress>) + Send> =
            Box::new(move |_chunk, get_progress| {
                if let Some(progress) = get_progress() {
                    emit_update(
                        &on_update_for_cb,
                        progress.output,
                        progress.truncation.truncated.then_some(progress.truncation),
                        progress
                            .full_output_path
                            .map(|path| path.to_string_lossy().into_owned()),
                    );
                }
            });

        let cwd = execution.cwd;
        let env = self.env.clone();
        let capture_opts = ShellCaptureOptions {
            shell: None,
            cwd: Some(cwd),
            env: Some(execution.env),
            inherit_env: execution.inherit_env,
            timeout,
            cancel: Some(&signal),
            on_chunk: Some(on_chunk),
            update_interval: Some(update_throttle()),
            return_execution_errors: true,
            persist_output: input.output_mode == ShellOutputMode::Compact,
        };

        // execute_shell_with_capture wants `&Arc<dyn ExecutionEnv>`.
        let capture = execute_shell_with_capture(&env, &command, capture_opts)
            .await
            .map_err(exec_err_to_agent)?;

        finish_shell_capture(&capture, input.output_mode, timeout, false)
    }
}

/// Emit a single `on_update` payload built from the given capture snapshot.
/// Mirrors the TS `onUpdate({ content:[{text:progress.output}], details:{...} })`.
pub(super) fn emit_update(
    on_update: &Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    output: String,
    truncation: Option<TruncationResult>,
    full_output_path: Option<String>,
) {
    let details = match (&truncation, &full_output_path) {
        (None, None) => serde_json::Value::Null,
        _ => {
            let details = BashToolDetails {
                truncation,
                full_output_path,
                exit_code: None,
                failure_kind: None,
                metrics: None,
            };
            serde_json::to_value(details).unwrap_or(serde_json::Value::Null)
        }
    };
    on_update(AgentToolResult {
        content: vec![TextContentOrImage::text(output)],
        details,
        ..Default::default()
    });
}

/// Build the terminal `(output_text, details)` from a capture, appending the
/// truncation continuation notice (mirrors the TS block).
///
/// When the command failed *and* its output was truncated, the tail alone is a
/// bad summary: the cause is usually printed first (a compiler's first error, the
/// first failing assertion). In that case a bounded head window is joined to the
/// tail, with the elided count and the full-log path stated. The window is refused
/// whenever it would not be strictly smaller than what it replaces, so this can
/// never inflate a result.
pub(crate) fn build_bash_output(capture: &ShellCaptureResult) -> (String, BashToolDetails) {
    let path_str = capture
        .full_output_path
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned());
    if capture.truncated && !matches!(capture.exit_code, Some(0) | None) {
        let totals = crate::output_window::OutputTotals {
            lines: capture.truncation.total_lines,
            bytes: capture.truncation.total_bytes,
        };
        let window = crate::output_window::build_window(
            &capture.head_output,
            &capture.output,
            totals,
            TruncationOptions::default(),
            crate::output_window::DEFAULT_HEAD_SHARE,
            path_str.as_deref(),
        );
        if let Some(window) = window {
            let truncation = crate::output_window::window_truncation(
                &window,
                totals,
                TruncationOptions::default(),
            );
            return (
                window.content,
                BashToolDetails {
                    truncation: Some(truncation),
                    full_output_path: path_str,
                    exit_code: None,
                    failure_kind: None,
                    metrics: None,
                },
            );
        }
    }
    let mut output_text = capture.output.clone();
    let details = if capture.truncated {
        let t = &capture.truncation;
        let start_line = t.total_lines.saturating_sub(t.output_lines) + 1;
        let end_line = t.total_lines;
        if t.last_line_partial {
            let last_line_size = format_size(capture.last_line_bytes);
            output_text.push_str(&format!(
                "\n\n[Showing last {} of line {end_line} (line is {last_line_size}). Full output: {}]",
                format_size(t.output_bytes),
                path_str.as_deref().unwrap_or("?")
            ));
        } else if matches!(
            t.truncated_by,
            Some(crate::truncate::TruncationLimit::Lines)
        ) {
            output_text.push_str(&format!(
                "\n\n[Showing lines {start_line}-{end_line} of {}. Full output: {}]",
                t.total_lines,
                path_str.as_deref().unwrap_or("?")
            ));
        } else {
            output_text.push_str(&format!(
                "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). Full output: {}]",
                t.total_lines,
                format_size(DEFAULT_MAX_BYTES),
                path_str.as_deref().unwrap_or("?")
            ));
        }
        BashToolDetails {
            truncation: Some(capture.truncation.clone()),
            full_output_path: path_str,
            exit_code: None,
            failure_kind: None,
            metrics: None,
        }
    } else {
        BashToolDetails {
            truncation: None,
            full_output_path: None,
            exit_code: None,
            failure_kind: None,
            metrics: None,
        }
    };
    (output_text, details)
}

/// Shared final formatting and failure classification for both shell tools.
pub(crate) fn finish_shell_capture(
    capture: &ShellCaptureResult,
    mode: ShellOutputMode,
    timeout: Option<f64>,
    powershell: bool,
) -> Result<AgentToolResult, AgentError> {
    let started = Instant::now();
    let (mut text, mut details) = build_bash_output(capture);
    details.exit_code = capture.exit_code;
    let mut compact_status = "raw";
    if mode == ShellOutputMode::Compact {
        compact_status = "unchanged";
        // A complete, successfully persisted log is mandatory before dropping
        // anything. The bounded retained view is never read back as a full log.
        if let Some(path) = capture.full_output_path.as_ref() {
            let path = path.to_string_lossy();
            if let Some(compact) = compact_output(&text, &path, capture.truncated) {
                text = compact;
                compact_status = if capture.truncated {
                    "partial"
                } else {
                    "compacted"
                };
            }
            // Also advertise recovery when compaction was not worth its marker.
            details.full_output_path = Some(path.into_owned());
        }
    }
    let (status, failure_kind) = if capture.cancelled {
        (
            Some(
                if powershell {
                    "PowerShell command aborted"
                } else {
                    "Command aborted"
                }
                .to_string(),
            ),
            Some(BashFailureKind::Cancelled),
        )
    } else if let Some(error) = &capture.execution_error {
        if error.code == ExecutionErrorCode::Timeout {
            let limit = timeout
                .map(|seconds| format!("after {seconds}s"))
                .unwrap_or_default();
            let message = if powershell && mode == ShellOutputMode::Raw {
                error.to_string()
            } else {
                format!("Command timed out {limit}")
            };
            (Some(message), Some(BashFailureKind::Timeout))
        } else {
            (Some(error.to_string()), Some(BashFailureKind::ToolFailed))
        }
    } else if let Some(code) = capture.exit_code.filter(|code| *code != 0) {
        let shell = if powershell { "PowerShell" } else { "Command" };
        (
            Some(format!("{shell} exited with code {code}")),
            Some(BashFailureKind::CommandFailed),
        )
    } else {
        (None, None)
    };
    // Preserve raw-mode error text, including PowerShell's historical empty
    // output separator. Compact mode keeps the recovery notice on failures.
    if mode == ShellOutputMode::Raw
        && (failure_kind == Some(BashFailureKind::ToolFailed)
            || (powershell && capture.cancelled)
            || (powershell && capture.execution_error.is_some()))
    {
        text.clear();
    }
    if let Some(status) = status {
        if !text.is_empty()
            || (powershell
                && mode == ShellOutputMode::Raw
                && failure_kind == Some(BashFailureKind::CommandFailed))
        {
            text.push_str("\n\n");
        }
        text.push_str(&status);
    } else if text.is_empty() || (powershell && text.trim().is_empty()) {
        text = "(no output)".into();
    }
    details.failure_kind = failure_kind;
    details.metrics = Some(ShellOutputMetrics {
        execution_ms: capture.metrics.execution_ms,
        result_format_ms: started.elapsed().as_secs_f64() * 1000.0,
        raw_bytes: capture.metrics.captured_bytes,
        displayed_bytes: text.len(),
        progress_updates: capture.metrics.progress_updates,
        compact_status,
    });
    let result = AgentToolResult {
        content: vec![TextContentOrImage::text(text)],
        details: serde_json::to_value(details).unwrap_or(serde_json::Value::Null),
        ..Default::default()
    };
    if failure_kind.is_some() {
        Err(AgentError::StructuredTool(Box::new(result)))
    } else {
        Ok(result)
    }
}

/// Resolve and validate the effective deadline before preparing a command.
fn resolve_timeout(timeout: Option<f64>, default_timeout: Option<f64>) -> Result<f64, AgentError> {
    let secs = timeout
        .or(default_timeout)
        .unwrap_or(DEFAULT_BASH_TIMEOUT_SECONDS);
    validate_timeout(secs)
}

/// Validate an explicit shell deadline before allocating or starting a process.
pub(super) fn validate_timeout(secs: f64) -> Result<f64, AgentError> {
    if !secs.is_finite() || secs <= 0.0 {
        return Err(AgentError::Tool(
            "Invalid timeout: must be a finite number of seconds".into(),
        ));
    }
    let max = 2_147_483_647.0 / 1000.0;
    if secs > max {
        return Err(AgentError::Tool(format!(
            "Invalid timeout: maximum is {max} seconds"
        )));
    }
    Ok(secs)
}

fn exec_err_to_agent(e: ExecutionError) -> AgentError {
    AgentError::Tool(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_and_null_timeout_use_tool_default() {
        for params in [
            serde_json::json!({ "command": "noop" }),
            serde_json::json!({ "command": "noop", "timeout": null }),
        ] {
            let input: BashInput = serde_json::from_value(params).unwrap();
            assert_eq!(resolve_timeout(input.timeout, None).unwrap(), 120.0);
        }
    }

    #[test]
    fn explicit_timeout_overrides_host_default() {
        assert_eq!(resolve_timeout(None, Some(30.0)).unwrap(), 30.0);
        assert_eq!(resolve_timeout(Some(300.0), Some(30.0)).unwrap(), 300.0);
        assert_eq!(resolve_timeout(Some(300.0), None).unwrap(), 300.0);
    }

    /// A synthetic capture: `total` numbered lines, of which `head_output` is the
    /// retained prefix and `output` the retained tail.
    fn capture_with(
        total: usize,
        exit_code: Option<i32>,
        path: Option<&str>,
    ) -> ShellCaptureResult {
        let all: Vec<String> = (0..total).map(|i| format!("L{i:06}")).collect();
        let head_output: String = all[..total.min(200)].join("\n");
        let tail: String = all[total.saturating_sub(2_000)..].join("\n");
        let truncation = crate::truncate::TruncationResult {
            content: tail.clone(),
            truncated: true,
            truncated_by: Some(crate::truncate::TruncationLimit::Lines),
            total_lines: total,
            total_bytes: total * 8,
            output_lines: 2_000,
            output_bytes: tail.len(),
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
        };
        ShellCaptureResult {
            output: tail,
            head_output,
            truncation,
            full_output_path: path.map(std::path::PathBuf::from),
            last_line_bytes: 7,
            exit_code,
            cancelled: false,
            truncated: true,
            execution_error: None,
            metrics: Default::default(),
        }
    }

    #[test]
    fn failed_truncated_command_shows_head_and_tail() {
        let (text, details) = build_bash_output(&capture_with(20_000, Some(1), Some("/tmp/o.log")));
        // The cause near the top and the verdict at the bottom both survive.
        assert!(text.starts_with("L000000"));
        assert!(text.contains("L019999"));
        // The elided middle is counted, not silently dropped.
        assert!(text.contains("lines elided, at least ("));
        // The complete log is still reachable.
        assert!(text.contains("[Full output: /tmp/o.log]"));
        let truncation = details.truncation.expect("truncation metadata");
        assert_eq!(truncation.total_lines, 20_000);
        assert_eq!(
            truncation.output_lines,
            200 + 2_000,
            "metadata must describe the window, not the tail"
        );
        assert_eq!(details.full_output_path.as_deref(), Some("/tmp/o.log"));
    }

    #[test]
    fn successful_truncated_command_stays_tail_only() {
        let (text, _) = build_bash_output(&capture_with(20_000, Some(0), Some("/tmp/o.log")));
        assert!(!text.starts_with("L000000"));
        assert!(text.starts_with("L018000"));
        assert!(text.contains("Showing lines 18001-20000 of 20000"));
        assert!(text.contains("Full output: /tmp/o.log"));
    }

    #[test]
    fn untruncated_failed_command_is_untouched() {
        let mut capture = capture_with(3, Some(1), None);
        capture.truncated = false;
        capture.output = "boom\n".to_string();
        capture.head_output = String::new();
        let (text, details) = build_bash_output(&capture);
        assert_eq!(text, "boom\n");
        assert!(details.truncation.is_none());
        assert!(details.full_output_path.is_none());
    }

    #[test]
    fn failed_command_without_retained_head_keeps_the_old_notice() {
        // Nothing was retained (output arrived as one oversized chunk), so the
        // existing tail notice is the honest message.
        let mut capture = capture_with(20_000, Some(1), Some("/tmp/o.log"));
        capture.head_output = String::new();
        let (text, details) = build_bash_output(&capture);
        assert!(text.contains("Showing lines 18001-20000 of 20000"));
        assert_eq!(details.truncation.map(|t| t.output_lines), Some(2_000));
    }

    #[test]
    fn effective_timeout_must_be_valid() {
        for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY, 2_147_484.0] {
            assert!(resolve_timeout(Some(invalid), None).is_err());
            assert!(resolve_timeout(None, Some(invalid)).is_err());
            assert_eq!(resolve_timeout(Some(10.0), Some(invalid)).unwrap(), 10.0);
        }
    }
}
