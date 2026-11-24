//! Mirrors `packages/coding-agent/src/core/tools/grep.ts` — the built-in `grep`
//! `AgentTool`. Searches file contents for a regex/literal pattern and returns
//! matching lines with `file:line: text` (and optional `-line-` context lines).
//! Read-only — goes through the `FileSystem` trait so it runs against both
//! `OsExecutionEnv` and `InMemoryExecutionEnv`.
//!
//! Uses ripgrep's matcher and streaming searcher through the filesystem backend.
//! Native searches honor ignore rules without first snapshotting the whole tree.

use std::sync::Arc;

use agent::agent_tool::AgentTool;
use agent::error::AgentError;
use agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use ai::types::Tool;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::env::ExecutionEnv;
use crate::path_utils::resolve_read_tool_path;
use crate::search::{ContentSearch, ContentSearchMode, MAX_SEARCH_CONTEXT, MAX_SEARCH_LIMIT};
use crate::tools::tool_context::ExecutionToolContext;
use crate::truncate::{
    format_size, TruncationLimit, TruncationResult, DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH,
};

/// Default max matches. Mirrors TS `grep.ts::DEFAULT_LIMIT`.
const DEFAULT_LIMIT: u32 = 100;

/// Input for the grep tool. Mirrors TS `GrepToolInput`. `camelCase` field names
/// match the TS wire schema (so `ignoreCase`/`ignore_case` deserialization lines
/// up with what the model emits).
#[derive(Debug, Clone, JsonSchema, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepInput {
    /// Search pattern (regex or literal string).
    pub pattern: String,
    /// Directory or file to search (default: current directory).
    #[serde(default)]
    pub path: Option<String>,
    /// Filter files by glob pattern, e.g. `*.ts` or `**/*.spec.ts`.
    #[serde(default)]
    pub glob: Option<String>,
    /// Case-insensitive search (default false).
    #[serde(default)]
    pub ignore_case: Option<bool>,
    /// Treat pattern as a literal string instead of regex (default false).
    #[serde(default)]
    pub literal: Option<bool>,
    /// Number of lines to show before and after each match (default 0).
    #[serde(default)]
    pub context: Option<u32>,
    /// Maximum results per page: matching lines in content mode, files otherwise
    /// (default 100, maximum 10000).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Output matching lines (content, default), filenames (files), or exact
    /// matching-line counts per file (count). Context applies only to content.
    #[serde(default)]
    pub mode: ContentSearchMode,
    /// Logical result offset (default 0). Reuse next_offset with unchanged
    /// inputs to continue. Pages rescan deterministically without a cache.
    #[serde(default)]
    pub offset: u64,
}

/// Structured details for the grep tool. Mirrors TS `GrepToolDetails`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GrepToolDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_limit_reached: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_truncated: Option<bool>,
    pub mode: ContentSearchMode,
    pub offset: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u64>,
    pub returned_results: usize,
    pub returned_matches: u64,
    /// Observed matches, not a total; files mode stops at the first per file.
    pub scanned_matches: u64,
    pub display_bytes: usize,
    pub search_complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counts_complete: Option<bool>,
    /// Truncation totals describe retained output, not unscanned content.
    pub totals_exact: bool,
}

/// Options for `create_grep_tool`. The TS tool carries an optional `operations`
/// bag for remote backends; the Rust port always uses the `FileSystem` trait, so
/// this is an empty forward-compat seam (parity with `ReadToolOptions`).
#[derive(Debug, Clone, Default)]
pub struct GrepToolOptions {}

/// The built-in `grep` tool. Holds an `Arc<dyn ExecutionEnv>` (read-only view).
pub struct GrepTool {
    schema: Tool,
    env: Arc<dyn ExecutionEnv>,
}

impl GrepTool {
    fn schema() -> Tool {
        let params = schemars::schema_for!(GrepInput);
        Tool {
            name: "grep".to_string(),
            description: format!(
                "Search file contents with ripgrep. mode=content returns matching lines with paths \
                 and line numbers (default), files returns each matching path once, and count returns \
                 matching-line counts per file. Native directory searches respect ignore rules. \
                 Results are deterministic, breadth-first filename order. Use offset=next_offset from \
                 details to continue unchanged inputs; pages rescan without a cache. Content context \
                 may overlap between pages; matching lines already displayed as context are consumed. \
                 Output is capped at {DEFAULT_LIMIT} results by default or {kb}KB. Long lines are \
                 truncated to {GREP_MAX_LINE_LENGTH} chars.",
                kb = DEFAULT_MAX_BYTES / 1024,
            ),
            parameters: ai::types::Schema::new(serde_json::to_value(params).unwrap_or_default()),
            constrained_sampling: None,
        }
    }
}

/// Build the `grep` tool from a context. Mirrors TS `createGrepTool`.
pub fn create_grep_tool(
    context: &ExecutionToolContext,
    options: Option<GrepToolOptions>,
) -> Arc<dyn AgentTool> {
    let _ = options; // forward-compat seam, no surface in v1
    Arc::new(GrepTool {
        schema: GrepTool::schema(),
        env: context.env().clone(),
    })
}

#[async_trait]
impl AgentTool for GrepTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "grep"
    }
    fn execution_mode(&self) -> agent::ToolExecutionMode {
        agent::ToolExecutionMode::Parallel
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: GrepInput = serde_json::from_value(params)
            .map_err(|e| AgentError::Validation(format!("grep input invalid: {e}")))?;

        let cancel = Some(&signal);
        let effective_limit = input.limit.unwrap_or(DEFAULT_LIMIT) as usize;
        let context_val = input.context.unwrap_or(0) as usize;
        if effective_limit == 0 || effective_limit > MAX_SEARCH_LIMIT {
            return Err(AgentError::Validation(format!(
                "grep limit must be between 1 and {MAX_SEARCH_LIMIT}"
            )));
        }
        if context_val > MAX_SEARCH_CONTEXT {
            return Err(AgentError::Validation(format!(
                "grep context must not exceed {MAX_SEARCH_CONTEXT}"
            )));
        }
        if input.offset.checked_add(effective_limit as u64).is_none() {
            return Err(AgentError::Validation(
                "grep offset plus limit overflows".into(),
            ));
        }

        let re = grep_regex::RegexMatcherBuilder::new()
            .case_insensitive(input.ignore_case.unwrap_or(false))
            .fixed_strings(input.literal.unwrap_or(false))
            .multi_line(true)
            .crlf(true)
            .build(&input.pattern)
            .map_err(|e| AgentError::Validation(format!("grep pattern invalid: {e}")))?;

        // Compile the optional glob filter. `has_separator` decides whether to
        // match the full relative path (`**/*.ts`) or just the basename (`*.ts`),
        // mirroring rg's basename-glob leniency.
        let glob: Option<(globset::GlobMatcher, bool)> = match input.glob.as_deref() {
            Some(g) if !g.is_empty() => {
                let compiled = globset::Glob::new(g)
                    .map_err(|e| AgentError::Validation(format!("grep glob invalid: {e}")))?
                    .compile_matcher();
                Some((compiled, g.contains('/')))
            }
            _ => None,
        };

        // Resolve the search path. A `.`/absent path resolves to the env cwd.
        let search_path =
            resolve_read_tool_path(&*self.env, input.path.as_deref().unwrap_or("."), cancel)
                .await
                .map_err(file_err_to_agent)?;

        let result = self
            .env
            .search_contents(
                &search_path,
                ContentSearch {
                    matcher: re,
                    glob,
                    context: context_val,
                    limit: effective_limit,
                    mode: input.mode,
                    offset: input.offset,
                },
                cancel,
            )
            .await
            .map_err(file_err_to_agent)?;
        check_cancel(&signal)?;
        let has_more = result.next_offset.is_some();
        let match_limit_reached = result.returned_results >= effective_limit
            && (has_more || input.mode == ContentSearchMode::Content);
        let mut output = if result.lines.is_empty() {
            if input.offset == 0 {
                "No matches found"
            } else {
                "No more matches found"
            }
            .to_owned()
        } else {
            result.lines.join("\n")
        };
        let mut details = GrepToolDetails {
            truncation: None,
            match_limit_reached: None,
            lines_truncated: result.lines_truncated.then_some(true),
            mode: input.mode,
            offset: input.offset,
            next_offset: result.next_offset,
            returned_results: result.returned_results,
            returned_matches: result.matches,
            scanned_matches: result.scanned_matches,
            display_bytes: 0,
            search_complete: !has_more && !result.partial_counts,
            counts_complete: (input.mode == ContentSearchMode::Count)
                .then_some(!result.partial_counts),
            totals_exact: input.offset == 0
                && !has_more
                && !result.partial_counts
                && !result.lines_truncated
                && input.mode != ContentSearchMode::Files,
        };
        let mut notices: Vec<String> = Vec::new();
        if match_limit_reached {
            let unit = if input.mode == ContentSearchMode::Content {
                "matches"
            } else {
                "files"
            };
            notices.push(format!("{effective_limit} {unit} limit reached"));
            details.match_limit_reached = Some(effective_limit as u32);
        }
        if result.byte_limit_reached {
            notices.push(format!(
                "{} limit reached; retained-output totals only",
                format_size(DEFAULT_MAX_BYTES)
            ));
            details.truncation = Some(TruncationResult {
                content: output.clone(),
                truncated: true,
                truncated_by: Some(TruncationLimit::Bytes),
                total_lines: result.lines.len(),
                total_bytes: result.bytes,
                output_lines: result.lines.len(),
                output_bytes: result.bytes,
                last_line_partial: false,
                first_line_exceeds_limit: false,
                max_lines: usize::MAX,
                max_bytes: DEFAULT_MAX_BYTES,
            });
            details.totals_exact = false;
        }
        if result.lines_truncated {
            notices.push(format!(
                "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
            ));
        }
        if let Some(offset) = result.next_offset {
            notices.push(format!(
                "Continue with offset={offset} (unchanged inputs; rescans without a cache)"
            ));
        }
        if !notices.is_empty() {
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }
        details.display_bytes = output.len();
        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(output)],
            details: serde_json::to_value(&details).unwrap_or(serde_json::Value::Null),
            ..Default::default()
        })
    }
}

fn check_cancel(signal: &CancellationToken) -> Result<(), AgentError> {
    if signal.is_cancelled() {
        return Err(file_err_to_agent(crate::error::FileError::aborted()));
    }
    Ok(())
}

/// Map a `FileError` into an `AgentError::Tool` (same shape as `read.rs`).
fn file_err_to_agent(e: crate::error::FileError) -> AgentError {
    AgentError::Tool(e.to_string())
}
