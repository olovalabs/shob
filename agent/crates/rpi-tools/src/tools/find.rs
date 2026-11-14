//! Mirrors `packages/coding-agent/src/core/tools/find.ts` — the built-in `find`
//! `AgentTool`. Searches for files by glob pattern and returns matching paths
//! (relative to the search directory, posix separators). Read-only — goes
//! through the `FileSystem` trait so it runs against both `OsExecutionEnv` and
//! `InMemoryExecutionEnv`.
//!
//! Divergence from TS (documented): the TS tool **shells out to `fd`** (with
//! gitignore-aware walking + `--full-path` rewrites + optional auto-download).
//! The Rust port implements the walk **in-process** via the `FileSystem` trait
//! and the `globset` crate, so it works against any `ExecutionEnv`. Trade-offs:
//! no full `.gitignore` awareness in v1 (only `.git/` directories are skipped;
//! revisit via the `ignore` crate for an `OsExecutionEnv`-only fast path), no
//! external binary, and a deterministic sorted BFS walk order rather than fd's
//! walk. The `**/`-prepend rule for path-containing patterns is ported; the
//! Windows `[/\\]` separator rewrite is unnecessary (we normalize to posix
//! internally). `relativizeFindResultPath` is ported directly.

use std::collections::VecDeque;
use std::path::Path;
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

/// Default max results. Mirrors TS `find.ts::DEFAULT_LIMIT`.
const DEFAULT_LIMIT: u32 = 1000;

/// Input for the find tool. Mirrors TS `FindToolInput`.
#[derive(Debug, Clone, JsonSchema, Deserialize)]
pub struct FindInput {
    /// Glob pattern to match files, e.g. `*.ts`, `**/*.json`, `src/**/*.spec.ts`.
    pub pattern: String,
    /// Directory to search in (default: current directory).
    #[serde(default)]
    pub path: Option<String>,
    /// Maximum number of results (default 1000).
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Structured details for the find tool. Mirrors TS `FindToolDetails`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FindToolDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_limit_reached: Option<u32>,
}

/// Options for `create_find_tool`. Forward-compat seam (the TS tool carries a
/// pluggable `operations` bag for remote backends; the Rust port always uses
/// the `FileSystem` trait).
#[derive(Debug, Clone, Default)]
pub struct FindToolOptions {}

/// The built-in `find` tool. Holds an `Arc<dyn ExecutionEnv>` (read-only view).
pub struct FindTool {
    schema: Tool,
    env: Arc<dyn ExecutionEnv>,
}

impl FindTool {
    fn schema() -> Tool {
        let params = schemars::schema_for!(FindInput);
        Tool {
            name: "find".to_string(),
            description: format!(
                "Search for files by glob pattern. Returns matching file paths relative to the \
                 search directory. Skips `.git/` directories. Output is truncated to {DEFAULT_LIMIT} \
                 results or {kb}KB (whichever is hit first).",
                kb = DEFAULT_MAX_BYTES / 1024
            ),
            parameters: rpi_ai::types::Schema::new(serde_json::to_value(params).unwrap_or_default()),
            constrained_sampling: None,
        }
    }
}

/// Build the `find` tool from a context. Mirrors TS `createFindTool`.
pub fn create_find_tool(
    context: &ExecutionToolContext,
    options: Option<FindToolOptions>,
) -> Arc<dyn AgentTool> {
    let _ = options; // forward-compat seam, no surface in v1
    Arc::new(FindTool {
        schema: FindTool::schema(),
        env: context.env().clone(),
    })
}

#[async_trait]
impl AgentTool for FindTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "find"
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: FindInput = serde_json::from_value(params)
            .map_err(|e| AgentError::Validation(format!("find input invalid: {e}")))?;

        let cancel = Some(&signal);
        let effective_limit = input.limit.unwrap_or(DEFAULT_LIMIT).max(1) as usize;

        // Resolve the search path (read-only + fuzzy variants).
        let search_path =
            resolve_read_tool_path(&*self.env, input.path.as_deref().unwrap_or("."), cancel)
                .await
                .map_err(file_err_to_agent)?;

        // Path-not-found (the TS custom-glob branch checks exists; the fd branch
        // lets fd error — we check up-front for a uniform in-process path).
        if !self
            .env
            .exists(&search_path, cancel)
            .await
            .map_err(file_err_to_agent)?
        {
            return Err(AgentError::Tool(format!("Path not found: {search_path}")));
        }

        // Compile the glob, applying the TS `**/`-prepend rewrite for
        // path-containing patterns. `**` alone matches everything.
        let effective_pattern = rewrite_pattern(&input.pattern);
        let matcher = globset::Glob::new(&effective_pattern)
            .map_err(|e| AgentError::Validation(format!("find pattern invalid: {e}")))?
            .compile_matcher();

        // Collect matching paths up to the limit.
        let mut collected: Vec<String> = Vec::new();
        let mut result_limit_reached = false;

        // BFS walk from the search root. Match against the relative posix path.
        let mut queue: VecDeque<(String, String)> = VecDeque::new();
        queue.push_back((search_path.clone(), String::new()));
        while let Some((abs_dir, rel_dir)) = queue.pop_front() {
            if collected.len() >= effective_limit {
                result_limit_reached = true;
                break;
            }
            let mut entries = match self.env.list_dir(&abs_dir, cancel).await {
                Ok(e) => e,
                Err(_) => continue, // unreadable dir — skip
            };
            entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            for entry in entries {
                if collected.len() >= effective_limit {
                    result_limit_reached = true;
                    break;
                }
                let child_abs = entry.path.to_string_lossy().into_owned();
                let rel = if rel_dir.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{rel_dir}/{}", entry.name)
                };
                match entry.kind {
                    FileKind::File => {
                        if matcher.is_match(&rel) {
                            collected.push(relativize(&rel));
                        }
                    }
                    FileKind::Directory => {
                        // Skip `.git/` (v1 has no full .gitignore support).
                        if entry.name == ".git" {
                            continue;
                        }
                        // Directories that match the glob are reported with a
                        // trailing slash (mirrors `relativizeFindResultPath`).
                        if matcher.is_match(&rel) {
                            let mut r = relativize(&rel);
                            if !r.ends_with('/') {
                                r.push('/');
                            }
                            collected.push(r);
                        }
                        queue.push_back((child_abs, rel));
                    }
                    FileKind::Symlink => {
                        // Skip symlinks during the walk (avoid loops).
                    }
                }
            }
        }

        if collected.is_empty() {
            return Ok(AgentToolResult {
                content: vec![TextContentOrImage::text("No files found matching pattern")],
                details: serde_json::Value::Null,
                ..Default::default()
            });
        }

        let raw_output = collected.join("\n");
        // Byte cap only — line cap disabled (result count already capped).
        let truncation = truncate_head(
            &raw_output,
            TruncationOptions {
                max_lines: Some(usize::MAX),
                max_bytes: None,
            },
        );
        let was_truncated = truncation.truncated;
        let mut output = truncation.content.clone();
        let mut details = FindToolDetails {
            truncation: None,
            result_limit_reached: None,
        };
        let mut notices: Vec<String> = Vec::new();
        if result_limit_reached {
            notices.push(format!(
                "{effective_limit} results limit reached. Use limit={} for more, or refine pattern",
                effective_limit * 2
            ));
            details.result_limit_reached = Some(effective_limit as u32);
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

/// Rewrite the glob pattern the way TS `find.ts` does for the `fd --full-path`
/// branch: a path-containing pattern (has a `/`) that isn't already rooted or
/// `**`-prefixed gets a `**/` prepend so it matches at any depth. Patterns with
/// no `/` are used as-is (basename match). `**` alone stays `**`.
///
/// The TS Windows `[/\\]` separator rewrite is intentionally NOT ported — we
/// normalize all relative paths to posix internally, so the matcher sees `/`.
fn rewrite_pattern(pattern: &str) -> String {
    if !pattern.contains('/') {
        return pattern.to_string();
    }
    if pattern == "**" || pattern.starts_with('/') || pattern.starts_with("**/") {
        return strip_native_sep(pattern);
    }
    strip_native_sep(&format!("**/{pattern}"))
}

/// Normalize any backslash separators to posix `/` (defensive — user patterns
/// on Windows may contain `\`).
fn strip_native_sep(p: &str) -> String {
    p.replace('\\', "/")
}

/// Relativize a walk result path. `rel` is already relative to the search root
/// and posix-normalized; this keeps a trailing `/` for directory matches
/// (mirrors `relativizeFindResultPath`'s trailing-separator preservation).
fn relativize(rel: &str) -> String {
    rel.to_string()
}

/// Filename component of a path string (for the no-`/` debugging path; unused
/// in the main walk but kept for parity/debugging clarity).
#[allow(dead_code)]
fn basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

/// Map a `FileError` into an `AgentError::Tool` (same shape as `read.rs`).
fn file_err_to_agent(e: crate::error::FileError) -> AgentError {
    AgentError::Tool(e.to_string())
}
