//! Mirrors `packages/agent/src/harness/tools/bash.ts` — the built-in `bash`
//! `AgentTool`. Drives `execute_shell_with_capture` with an incremental
//! checkpoint throttle, and converts ALL failures (non-zero exit, timeout,
//! abort, executionError) into `Err(AgentError)` — the agent loop encodes those
//! into an error `ToolResultMessage`.
//!
//! The first chunk is emitted immediately. Later checkpoints are coalesced into
//! a short window (16 ms by default, overridable with
//! `RPI_BASH_UPDATE_THROTTLE_MS`); a final flush always follows capture
//! resolution.

use futures::future::BoxFuture;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use rpi_ai::types::{ConstrainedSamplingConfig, ConstrainedStrictness, Tool};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::env::ExecutionEnv;
use crate::error::{ExecutionError, ExecutionErrorCode};
use crate::shell_output::{
    execute_shell_with_capture, ShellCaptureOptions, ShellCaptureProgress, ShellCaptureResult,
};
use crate::tools::tool_context::ExecutionToolContext;
use crate::truncate::{format_size, TruncationResult, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

/// Default throttle window for bash `on_update` flushes. The first chunk is
/// emitted immediately; this window only coalesces later checkpoints.
const BASH_UPDATE_THROTTLE_MS: u64 = 16;

/// The active checkpoint window, resolved once. Reading the environment on every
/// chunk allocated a `String` and ran a lookup in the read loop.
pub(super) fn update_throttle() -> Duration {
    static THROTTLE: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *THROTTLE.get_or_init(|| {
        std::env::var("RPI_BASH_UPDATE_THROTTLE_MS")
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
                 Defaults to a {default_timeout}-second timeout; provide timeout in seconds to override it. \
                 A non-zero exit code is a command result: probe commands such as grep (no match) or \
                 git config --get (missing key) legitimately return 1. Read the output before deciding \
                 whether to retry; details carries exit_code and failure_kind.",
                kb = DEFAULT_MAX_BYTES / 1024
            ),
            parameters: rpi_ai::types::Schema::new(params),
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
        };

        // execute_shell_with_capture wants `&Arc<dyn ExecutionEnv>`.
        let capture = execute_shell_with_capture(&env, &command, capture_opts)
            .await
            .map_err(exec_err_to_agent)?;

        // Build the terminal output text + details, applying truncation notices.
        let (output_text, mut details) = build_bash_output(&capture);
        details.exit_code = capture.exit_code;

        // A failed call still returns `isError=true` (unchanged), but carries
        // structured `details` through `AgentError::StructuredTool` so a
        // caller can tell "the command returned 1" from "the tool broke".
        // Plain `AgentError::Tool` is only used where no AgentToolResult can be
        // built (validation errors earlier in execute).
        let append_status = |status: String| -> String {
            if output_text.is_empty() {
                status
            } else {
                format!("{output_text}\n\n{status}")
            }
        };
        // A small helper (not an FnMut closure) so `details` is borrowed only
        // for the duration of each call and `output_text` can be used normally.
        fn structured_failure(
            message: String,
            kind: BashFailureKind,
            details: &mut BashToolDetails,
        ) -> AgentError {
            details.failure_kind = Some(kind);
            let details_value = serde_json::to_value(&*details).unwrap_or(serde_json::Value::Null);
            AgentError::StructuredTool(Box::new(AgentToolResult {
                content: vec![TextContentOrImage::text(message)],
                details: details_value,
                ..Default::default()
            }))
        }
        if capture.cancelled {
            return Err(structured_failure(
                append_status("Command aborted".into()),
                BashFailureKind::Cancelled,
                &mut details,
            ));
        }
        if let Some(ee) = &capture.execution_error {
            if ee.code == ExecutionErrorCode::Timeout {
                // `timeout` is `Option<f64>` in seconds; `{timeout:?}` used to render
                // `Some(2.0)`, so the message read "timed out after Some(2.0) seconds".
                let limit = match timeout {
                    Some(secs) => format!("after {secs}s"),
                    None => "".to_string(),
                };
                return Err(structured_failure(
                    append_status(format!("Command timed out {limit}")),
                    BashFailureKind::Timeout,
                    &mut details,
                ));
            }
            return Err(structured_failure(
                ee.to_string(),
                BashFailureKind::ToolFailed,
                &mut details,
            ));
        }
        if let Some(code) = capture.exit_code {
            if code != 0 {
                return Err(structured_failure(
                    append_status(format!("Command exited with code {code}")),
                    BashFailureKind::CommandFailed,
                    &mut details,
                ));
            }
        }
        let text = if output_text.is_empty() {
            "(no output)".to_string()
        } else {
            output_text
        };
        let details_value = serde_json::to_value(&details).unwrap_or(serde_json::Value::Null);
        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(text)],
            details: details_value,
            ..Default::default()
        })
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
fn build_bash_output(capture: &ShellCaptureResult) -> (String, BashToolDetails) {
    let mut output_text = capture.output.clone();
    let details = if capture.truncated {
        let t = &capture.truncation;
        let start_line = t.total_lines.saturating_sub(t.output_lines) + 1;
        let end_line = t.total_lines;
        let path_str = capture
            .full_output_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
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
        }
    } else {
        BashToolDetails {
            truncation: None,
            full_output_path: None,
            exit_code: None,
            failure_kind: None,
        }
    };
    (output_text, details)
}

/// Resolve and validate the effective deadline before preparing a command.
fn resolve_timeout(timeout: Option<f64>, default_timeout: Option<f64>) -> Result<f64, AgentError> {
    let secs = timeout
        .or(default_timeout)
        .unwrap_or(DEFAULT_BASH_TIMEOUT_SECONDS);
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

    #[test]
    fn effective_timeout_must_be_valid() {
        for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY, 2_147_484.0] {
            assert!(resolve_timeout(Some(invalid), None).is_err());
            assert!(resolve_timeout(None, Some(invalid)).is_err());
            assert_eq!(resolve_timeout(Some(10.0), Some(invalid)).unwrap(), 10.0);
        }
    }
}
