//! Mirrors `packages/coding-agent/src/core/tools/ls.ts` — the built-in `ls`
//! `AgentTool`. Lists directory entries (dotfiles included), sorted
//! case-insensitively, with a `/` suffix on directories. Read-only — goes
//! through the `FileSystem` trait so it runs against both `OsExecutionEnv` and
//! `InMemoryExecutionEnv`.
//!
//! Divergence from TS (documented): the TS tool delegates to pluggable
//! `LsOperations` (`readdir` + per-entry `stat`); the Rust port uses
//! `FileSystem::list_dir`, which already returns `FileInfo` with a `kind`, so
//! the per-entry stat call is folded into the listing (no second round-trip).
//! Unstatable entries are therefore never silently skipped here — `list_dir`
//! either lists them or the whole call fails. Output shape, sorting, limits,
//! and truncation match TS exactly.

use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use rpi_ai::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::env::ExecutionEnv;
use crate::env::FileKind;
use crate::path_utils::resolve_read_tool_path;
use crate::tools::tool_context::ExecutionToolContext;
use crate::truncate::{
    format_size, truncate_head, TruncationOptions, TruncationResult, DEFAULT_MAX_BYTES,
};

/// Default max entries. Mirrors TS `ls.ts::DEFAULT_LIMIT`.
const DEFAULT_LIMIT: u32 = 500;

/// Input for the ls tool. Mirrors TS `LsToolInput`.
#[derive(Debug, Clone, JsonSchema, Deserialize)]
pub struct LsInput {
    /// Directory to list (default: current directory).
    #[serde(default)]
    pub path: Option<String>,
    /// Maximum number of entries to return (default 500).
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Structured details for the ls tool. Mirrors TS `LsToolDetails`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LsToolDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_limit_reached: Option<u32>,
}

/// Options for `create_ls_tool`. The TS tool carries an optional `operations`
/// bag for remote backends; the Rust port has no pluggable backend (it always
/// goes through the `FileSystem` trait), so this is an empty forward-compat
/// seam kept for parity with `ReadToolOptions`.
#[derive(Debug, Clone, Default)]
pub struct LsToolOptions {}

/// The built-in `ls` tool. Holds an `Arc<dyn ExecutionEnv>` (read-only view).
pub struct LsTool {
    schema: Tool,
    env: Arc<dyn ExecutionEnv>,
}

impl LsTool {
    fn schema() -> Tool {
        let params = schemars::schema_for!(LsInput);
        Tool {
            name: "ls".to_string(),
            description: format!(
                "List directory contents. Returns entries sorted alphabetically, with a '/' suffix \
                 for directories. Includes dotfiles. Output is truncated to {DEFAULT_LIMIT} entries \
                 or {kb}KB (whichever is hit first).",
                kb = DEFAULT_MAX_BYTES / 1024
            ),
            parameters: rpi_ai::types::Schema::new(serde_json::to_value(params).unwrap_or_default()),
            constrained_sampling: None,
        }
    }
}

/// Build the `ls` tool from a context. Mirrors TS `createLsTool`.
pub fn create_ls_tool(
    context: &ExecutionToolContext,
    options: Option<LsToolOptions>,
) -> Arc<dyn AgentTool> {
    let _ = options; // no options surface in v1 (forward-compat seam)
    Arc::new(LsTool {
        schema: LsTool::schema(),
        env: context.env().clone(),
    })
}

#[async_trait]
impl AgentTool for LsTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "ls"
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: LsInput = serde_json::from_value(params)
            .map_err(|e| AgentError::Validation(format!("ls input invalid: {e}")))?;

        let cancel = Some(&signal);
        let effective_limit = input.limit.unwrap_or(DEFAULT_LIMIT).max(1) as usize;

        // Resolve the directory path (read-only resolution + fuzzy variants).
        let dir_path =
            resolve_read_tool_path(&*self.env, input.path.as_deref().unwrap_or("."), cancel)
                .await
                .map_err(file_err_to_agent)?;

        // Path-not-found.
        if !self
            .env
            .exists(&dir_path, cancel)
            .await
            .map_err(file_err_to_agent)?
        {
            return Err(AgentError::Tool(format!("Path not found: {dir_path}")));
        }
        // Must be a directory.
        let info = self
            .env
            .file_info(&dir_path, cancel)
            .await
            .map_err(file_err_to_agent)?;
        if info.kind != FileKind::Directory {
            return Err(AgentError::Tool(format!("Not a directory: {dir_path}")));
        }

        // Read entries. (TS wraps readdir in try/catch → "Cannot read directory";
        // the env surfaces that as a FileError which we map here.)
        let mut entries = self
            .env
            .list_dir(&dir_path, cancel)
            .await
            .map_err(file_err_to_agent)?;

        // Sort alphabetically, case-insensitive. Mirrors TS
        // `entries.sort((a,b) => a.toLowerCase().localeCompare(b.toLowerCase()))`.
        entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

        // Format with directory indicators.
        let mut results: Vec<String> = Vec::new();
        let mut entry_limit_reached = false;
        for entry in &entries {
            if results.len() >= effective_limit {
                entry_limit_reached = true;
                break;
            }
            let suffix = if entry.kind == FileKind::Directory {
                "/"
            } else {
                ""
            };
            results.push(format!("{}{suffix}", entry.name));
        }

        if results.is_empty() {
            return Ok(AgentToolResult {
                content: vec![TextContentOrImage::text("(empty directory)")],
                details: serde_json::Value::Null,
                ..Default::default()
            });
        }

        let raw_output = results.join("\n");
        // Byte cap only — line cap disabled (entry count already capped).
        let truncation = truncate_head(
            &raw_output,
            TruncationOptions {
                max_lines: Some(usize::MAX),
                max_bytes: None,
            },
        );
        let was_truncated = truncation.truncated;
        let mut output = truncation.content.clone();
        let mut details = LsToolDetails {
            truncation: None,
            entry_limit_reached: None,
        };
        let mut notices: Vec<String> = Vec::new();
        if entry_limit_reached {
            notices.push(format!(
                "{effective_limit} entries limit reached. Use limit={} for more",
                effective_limit * 2
            ));
            details.entry_limit_reached = Some(effective_limit as u32);
        }
        if was_truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
            details.truncation = Some(truncation);
        }
        if !notices.is_empty() {
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }

        let details_value = serde_json::to_value(&details).unwrap_or(serde_json::Value::Null);
        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(output)],
            details: details_value,
            ..Default::default()
        })
    }
}

/// Map a `FileError` into an `AgentError::Tool` (same shape as `read.rs`).
fn file_err_to_agent(e: crate::error::FileError) -> AgentError {
    AgentError::Tool(e.to_string())
}
