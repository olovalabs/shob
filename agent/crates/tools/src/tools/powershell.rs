//! Optional Windows PowerShell tool, matching upstream's `powershell` tool.
//! The command is passed through `-EncodedCommand` so quotes and newlines in
//! user input cannot be reinterpreted by the outer Bash transport.

use std::sync::Arc;

use agent::agent_tool::AgentTool;
use agent::error::AgentError;
use agent::types::{AgentToolResult, ToolResultPartial};
use ai::types::Tool;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::bash::{finish_shell_capture, ShellOutputMode};
use crate::env::ShellOverride;
use crate::shell_output::{execute_shell_with_capture, ShellCaptureOptions};
use crate::tools::tool_context::ExecutionToolContext;

#[derive(Debug, Clone, JsonSchema, Deserialize)]
pub struct PowerShellInput {
    pub command: String,
    #[serde(default)]
    pub timeout: Option<f64>,
    /// Output formatting: raw (default) preserves output; compact conservatively
    /// collapses recognized Cargo/pytest/Go/JS progress and saves the full log.
    #[serde(default)]
    pub output_mode: ShellOutputMode,
}

pub struct PowerShellTool {
    schema: Tool,
    env: Arc<dyn crate::env::ExecutionEnv>,
}

impl PowerShellTool {
    fn schema() -> Tool {
        Tool {
            name: "powershell".into(),
            description: "Execute a PowerShell command on Windows. Returns stdout and stderr. output_mode defaults to raw; compact collapses recognized Cargo, pytest, Go, and JS passing/progress lines, retains diagnostics, and saves a complete sanitized log. Truncated output includes a full-log path."
                .into(),
            parameters: ai::types::Schema::new(
                serde_json::to_value(schemars::schema_for!(PowerShellInput)).unwrap_or_default(),
            ),
            constrained_sampling: None,
        }
    }
}

pub fn create_powershell_tool(context: &ExecutionToolContext) -> Arc<dyn AgentTool> {
    Arc::new(PowerShellTool {
        schema: PowerShellTool::schema(),
        env: context.env().clone(),
    })
}

#[async_trait]
impl AgentTool for PowerShellTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "powershell"
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
        if !cfg!(windows) {
            return Err(AgentError::Tool(
                "PowerShell tool is only available on Windows".into(),
            ));
        }
        let input: PowerShellInput = serde_json::from_value(params)
            .map_err(|e| AgentError::Validation(format!("powershell input invalid: {e}")))?;
        let utf16: Vec<u8> = input
            .command
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let encoded = STANDARD.encode(utf16);
        let command = encoded;
        let candidate = std::env::var_os("ProgramFiles")
            .map(|p| std::path::PathBuf::from(p).join("PowerShell/7/pwsh.exe"));
        let program = match candidate {
            Some(path) if tokio::fs::metadata(&path).await.is_ok() => path,
            _ => std::path::PathBuf::from("powershell.exe"),
        };
        super::bash::emit_update(&on_update, String::new(), None, None);
        let progress_update = on_update.clone();
        let capture = execute_shell_with_capture(
            &self.env,
            &command,
            ShellCaptureOptions {
                shell: Some(ShellOverride {
                    program,
                    args: vec![
                        "-NoLogo".into(),
                        "-NoProfile".into(),
                        "-NonInteractive".into(),
                        "-ExecutionPolicy".into(),
                        "Bypass".into(),
                        "-EncodedCommand".into(),
                    ],
                }),
                cwd: Some(self.env.cwd().to_path_buf()),
                env: None,
                inherit_env: true,
                timeout: input.timeout,
                cancel: Some(&signal),
                on_chunk: Some(Box::new(move |_, progress| {
                    if let Some(progress) = progress() {
                        super::bash::emit_update(
                            &progress_update,
                            progress.output,
                            progress.truncation.truncated.then_some(progress.truncation),
                            progress
                                .full_output_path
                                .map(|path| path.to_string_lossy().into_owned()),
                        );
                    }
                })),
                update_interval: Some(super::bash::update_throttle()),
                return_execution_errors: true,
                persist_output: input.output_mode == ShellOutputMode::Compact,
            },
        )
        .await
        .map_err(|e| AgentError::Tool(e.to_string()))?;
        finish_shell_capture(&capture, input.output_mode, input.timeout, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileSystem, InMemoryExecutionEnv, ShellScript};

    #[test]
    fn output_mode_defaults_to_raw_and_schema_advertises_compact() {
        let input: PowerShellInput =
            serde_json::from_value(serde_json::json!({"command": "noop"})).unwrap();
        assert_eq!(input.output_mode, ShellOutputMode::Raw);
        let schema = serde_json::to_value(schemars::schema_for!(PowerShellInput)).unwrap();
        assert!(schema.to_string().contains("compact"));
        assert!(schema["properties"]["output_mode"]["description"]
            .as_str()
            .unwrap()
            .contains("full log"));
        assert!(serde_json::from_value::<PowerShellInput>(
            serde_json::json!({"command": "noop", "output_mode": "unsupported"})
        )
        .is_err());
    }

    #[tokio::test]
    async fn shared_powershell_formatter_keeps_failure_and_complete_recovery_log() {
        let env = Arc::new(InMemoryExecutionEnv::with_cwd("/tmp/work".into()));
        let raw = format!(
            "{}test result: FAILED. 80 passed; 1 failed\n\nAssertionError at tests/example.py:42\n",
            "test example::passes ... ok\n".repeat(80)
        );
        env.register_shell(
            "encoded-command",
            ShellScript {
                stdout: raw.clone(),
                stderr: String::new(),
                exit_code: 9,
                delay_ms: None,
            },
        )
        .await;
        let execution_env: Arc<dyn crate::env::ExecutionEnv> = env.clone();
        let capture = execute_shell_with_capture(
            &execution_env,
            "encoded-command",
            ShellCaptureOptions {
                persist_output: true,
                return_execution_errors: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let error =
            finish_shell_capture(&capture, ShellOutputMode::Compact, None, true).unwrap_err();
        let AgentError::StructuredTool(result) = error else {
            panic!("structured failure required")
        };
        let agent::types::TextContentOrImage::Text(text) = &result.content[0] else {
            panic!("text required")
        };
        assert!(text.text.contains("PowerShell exited with code 9"));
        assert!(text.text.contains("AssertionError at tests/example.py:42"));
        assert!(text
            .text
            .contains("test result: FAILED. 80 passed; 1 failed"));
        assert_eq!(text.text.matches("Full output:").count(), 1);
        assert_eq!(result.details["exit_code"], 9);
        assert_eq!(result.details["failure_kind"], "command_failed");
        assert_eq!(result.details["metrics"]["compact_status"], "compacted");
        assert_eq!(result.details["metrics"]["raw_bytes"], raw.len());
        assert_eq!(
            result.details["metrics"]["displayed_bytes"],
            text.text.len()
        );
        let path = result.details["full_output_path"].as_str().unwrap();
        assert_eq!(env.read_text_file(path, None).await.unwrap(), raw);
    }
}
