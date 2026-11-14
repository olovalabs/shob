//! Built-in `docs` tool for looking up rpi usage documentation.
//!
//! Pi ships its coding-agent documentation with the CLI and lets the model
//! reach it through the normal read/documentation flow. rpi keeps the same
//! model-facing behavior with a small, read-only topic index. The selected
//! Markdown pages are embedded at compile time so an installed binary does
//! not depend on the source checkout being present.

use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, ToolExecutionMode, ToolResultPartial};
use rpi_ai::types::Tool;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

const MAX_SEARCH_RESULTS: usize = 8;
const MAX_RESULT_CHARS: usize = 18_000;

struct DocPage {
    topic: &'static str,
    description: &'static str,
    content: &'static str,
    aliases: &'static [&'static str],
}

static DOCS: &[DocPage] = &[
    DocPage {
        topic: "authoring",
        description: "Rust cdylib 扩展开发最佳实践：统一 C ABI 入口、工具生命周期、事件处理器、资源发现、rpi dev 开发、rpi-extcore 复用边界与发布检查清单",
        content: include_str!("../embedded-docs/extension-authoring.md"),
        aliases: &["package-authoring", "extension-authoring", "create-package", "create-extension"],
    },
    DocPage {
        topic: "guide",
        description: "完整使用手册：安装、模型、CLI、.rpi 资源、Rust 扩展、SDK、排错和发布",
        content: include_str!("../embedded-docs/user-guide.md"),
        aliases: &["manual", "user-guide", "cli", "quickstart", "package", "packages", "extension", "extensions"],
    },
    DocPage {
        topic: "overview",
        description: "rpi installation, built-in tools, configuration, extensions, and release basics",
        content: include_str!("../embedded-docs/README.md"),
        aliases: &["readme", "getting-started", "start", "usage"],
    },
    DocPage {
        topic: "agent",
        description: "用 rpi SDK 创建自己的 Agent 项目的默认结构与规范：目录布局、内嵌 Agent、cdylib 适配、工具单一登记点、prompts/skills、配置与开发循环",
        content: include_str!("../embedded-docs/agent-project.md"),
        aliases: &[
            "agent-project",
            "project-layout",
            "create-agent",
            "scaffold",
            "project-structure",
        ],
    },
    DocPage {
        topic: "architecture",
        description: "rpi crate layering, agent loop, provider, harness, sessions, and extension boundaries",
        content: include_str!("../embedded-docs/architecture.md"),
        aliases: &["design", "crates", "sdk"],
    },
    DocPage {
        topic: "debugging",
        description: "Rust cdylib 扩展编写（统一 C ABI、工具生命周期、事件处理器）与 agent/扩展调试（事件日志、rpi dev-local、常见失败定位）",
        content: include_str!("../embedded-docs/rust-debugging.md"),
        aliases: &["rust", "rust-debug", "extension-debug", "troubleshoot", "debug"],
    },
];

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DocsInput {
    /// Topic to open. Omit it, or use `list`, to list available topics.
    #[serde(default)]
    pub topic: Option<String>,
    /// Optional case-insensitive text to find inside the selected topic.
    #[serde(default)]
    pub query: Option<String>,
}

pub struct DocsTool {
    schema: Tool,
}

impl DocsTool {
    fn new() -> Self {
        let params = schemars::schema_for!(DocsInput);
        Self {
            schema: Tool {
                name: "docs".to_string(),
                description: "Look up rpi usage documentation. Omit topic (or use topic=list) to list topics; pass a topic such as guide, authoring, agent, debugging, architecture, or overview. Add query to find relevant sections. Use this before guessing rpi commands, creating an agent project or extension, extension APIs, or .rpi configuration.".to_string(),
                parameters: rpi_ai::types::Schema::new(
                    serde_json::to_value(params).unwrap_or_default(),
                ),
                constrained_sampling: None,
            },
        }
    }
}

pub fn create_docs_tool() -> Arc<dyn AgentTool> {
    Arc::new(DocsTool::new())
}

#[async_trait]
impl AgentTool for DocsTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        "docs"
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: DocsInput = serde_json::from_value(params)
            .map_err(|error| AgentError::Validation(format!("docs input invalid: {error}")))?;
        if signal.is_cancelled() {
            return Err(AgentError::Tool("docs lookup cancelled".into()));
        }

        let topic = input.topic.as_deref().unwrap_or("list").trim();
        if topic.is_empty() || topic.eq_ignore_ascii_case("list") || topic == "*" {
            return Ok(AgentToolResult::text(format_catalog()));
        }

        let page = DOCS
            .iter()
            .find(|page| {
                page.topic.eq_ignore_ascii_case(topic)
                    || page
                        .aliases
                        .iter()
                        .any(|alias| alias.eq_ignore_ascii_case(topic))
            })
            .ok_or_else(|| {
                AgentError::Tool(format!(
                    "Unknown rpi docs topic `{topic}`. Available topics: {}",
                    DOCS.iter()
                        .map(|page| page.topic)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;

        let query = input
            .query
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let body = match query {
            Some(query) => search_with_fallback(page, query),
            None => truncate_result(page.content),
        };
        Ok(AgentToolResult::text(format!(
            "# rpi docs: {}\n\n{}",
            page.topic, body
        )))
    }
}

fn format_catalog() -> String {
    let mut out = String::from("Available rpi documentation topics:\n");
    for page in DOCS {
        out.push_str(&format!("- {}: {}\n", page.topic, page.description));
    }
    out.push_str("\nUse docs with {\"topic\": \"agent\"} or add {\"query\": \"AgentBuilder\"} for a focused lookup.");
    out
}

/// Split a free-text query into lower-cased terms. A long Chinese query or a
/// compound phrase used to miss every line because `search_page` did a single
/// whole-string substring test (fork issue #14 §3: 32 of 51 `docs` calls
/// returned `No matches`). Both whitespace and common punctuation are term
/// separators; CJK characters are kept as runs so `资源发现` stays one term.
fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| c.is_whitespace() || matches!(c, '/' | ',' | ';' | ':' | '(' | ')' | '|'))
        .filter_map(|part| {
            let term = part
                .trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase();
            (!term.is_empty()).then_some(term)
        })
        .collect()
}

/// A line matches when **every** term appears in it. AND is deliberate: a
/// multi-word query is a narrowing request, and OR would flood the result with
/// lines that contain only `rpi` or only `config`.
fn line_matches(line: &str, terms: &[String]) -> bool {
    let lower = line.to_lowercase();
    terms.iter().all(|term| lower.contains(term))
}

fn search_page(page: &DocPage, query: &str) -> String {
    let terms = query_terms(query);
    let lines: Vec<&str> = page.content.lines().collect();
    let mut selected = Vec::new();
    if !terms.is_empty() {
        for (index, line) in lines.iter().enumerate() {
            if !line_matches(line, &terms) {
                continue;
            }
            let start = index.saturating_sub(2);
            let end = (index + 3).min(lines.len());
            for line_no in start..end {
                if !selected.contains(&line_no) {
                    selected.push(line_no);
                }
            }
            if selected.len() >= MAX_SEARCH_RESULTS * 5 {
                break;
            }
        }
    }
    if selected.is_empty() {
        return format!(
            "No matches for `{query}` in `{}`. Try docs with topic=list or a broader query.",
            page.topic
        );
    }
    let mut out = format!("Matches for `{query}` in `{}`:\n\n", page.topic);
    for line_no in selected {
        out.push_str(&format!("{:>5}: {}\n", line_no + 1, lines[line_no]));
        if out.len() >= MAX_RESULT_CHARS {
            out.push_str("\n[Result truncated; run another focused docs query.]\n");
            break;
        }
    }
    out
}

/// When a query has no hit in the selected topic, look through the other
/// topics and return the first page that does. The output says which topic was
/// used so a caller never mistakes a fallback hit for a hit in `page`.
fn search_with_fallback(page: &DocPage, query: &str) -> String {
    let direct = search_page(page, query);
    if !direct.starts_with("No matches for `") {
        return direct;
    }
    let terms = query_terms(query);
    if terms.is_empty() {
        return direct;
    }
    for other in DOCS
        .iter()
        .filter(|candidate| candidate.topic != page.topic)
    {
        if other.content.lines().any(|line| line_matches(line, &terms)) {
            let body = search_page(other, query);
            return format!(
                "No matches for `{query}` in `{}`. Found it in `{}` instead:\n\n{body}",
                page.topic, other.topic
            );
        }
    }
    direct
}

fn truncate_result(content: &str) -> String {
    if content.len() <= MAX_RESULT_CHARS {
        return content.to_string();
    }
    let mut end = MAX_RESULT_CHARS;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n[Document truncated; use docs with a query for a focused section.]",
        &content[..end]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn execute(input: serde_json::Value) -> String {
        let tool = DocsTool::new();
        let result = tool
            .execute("test", input, CancellationToken::new(), Arc::new(|_| {}))
            .await
            .unwrap();
        match &result.content[0] {
            rpi_agent::types::TextContentOrImage::Text(text) => text.text.clone(),
            _ => panic!("docs returned an image"),
        }
    }

    #[tokio::test]
    async fn lists_topics() {
        let output = execute(serde_json::json!({})).await;
        assert!(output.contains("overview"));
        assert!(output.contains("guide"));
        assert!(output.contains("authoring"));
        assert!(output.contains("- agent:"));
        assert!(output.contains("debugging"));
        assert!(output.contains("统一 C ABI"));
        assert!(!output.contains("ABI v3/v2"));
        assert!(!output.contains("- extensions:"));
        assert!(!output.contains("- compatibility:"));
    }

    #[tokio::test]
    async fn returns_rust_debugging_guide() {
        let output = execute(serde_json::json!({"topic": "debugging", "query": "rpi dev"})).await;
        assert!(output.contains("rpi dev"));
        assert!(output.contains("cdylib"));
    }

    #[tokio::test]
    async fn returns_runtime_diagnostics_guide() {
        let output = execute(serde_json::json!({
            "topic": "debugging",
            "query": "RPI_FRAME_DIAGNOSTICS"
        }))
        .await;
        assert!(output.contains("RPI_FRAME_DIAGNOSTICS"));
        assert!(output.contains("frame writer"));
    }

    #[tokio::test]
    async fn returns_runtime_diagnostics_from_overview() {
        let output = execute(serde_json::json!({
            "topic": "overview",
            "query": "RPI_AGENT_DIAGNOSTICS"
        }))
        .await;
        assert!(output.contains("RPI_AGENT_DIAGNOSTICS"));
        assert!(output.contains("latencies"));
    }

    #[tokio::test]
    async fn returns_complete_user_guide() {
        let output = execute(serde_json::json!({"topic": "guide", "query": "cdylib"})).await;
        assert!(output.contains("cdylib"));
        assert!(output.contains("Rust"));
    }

    #[tokio::test]
    async fn returns_extension_authoring_practices() {
        let output = execute(serde_json::json!({"topic": "authoring", "query": "rpi dev"})).await;
        assert!(output.contains("rpi dev"));

        // The cdylib build contract is a separate section from the `rpi`
        // dev workflow text, so query for it directly rather than relying
        // on both terms co-occurring in one excerpt window.
        let cdylib = execute(serde_json::json!({"topic": "authoring", "query": "cdylib"})).await;
        assert!(cdylib.contains("cdylib"));
    }

    #[tokio::test]
    async fn returns_extcore_reuse_guidance() {
        let output = execute(serde_json::json!({
            "topic": "authoring",
            "query": "rpi-extcore"
        }))
        .await;
        assert!(output.contains("rpi-extcore"));
        assert!(output.contains("rpi-plugin-sdk"));
    }

    #[tokio::test]
    async fn returns_agent_project_guide() {
        let output = execute(serde_json::json!({
            "topic": "agent",
            "query": "AgentBuilder"
        }))
        .await;
        assert!(output.contains("AgentBuilder"));
        assert!(output.contains("Matches for"));
    }

    #[tokio::test]
    async fn agent_aliases_resolve() {
        let output = execute(serde_json::json!({"topic": "create-agent"})).await;
        assert!(output.contains("rpi Agent 项目结构创建指南"));
    }

    #[tokio::test]
    async fn package_and_extension_aliases_resolve() {
        for topic in ["package", "packages", "extension", "extensions", "PACKAGES"] {
            let output = execute(serde_json::json!({"topic": topic, "query": "packages"})).await;
            assert!(output.starts_with("# rpi docs: guide"));
            assert!(output.contains("Matches for"));
        }
    }

    #[tokio::test]
    async fn rejects_unknown_topic() {
        let error = DocsTool::new()
            .execute(
                "test",
                serde_json::json!({"topic": "missing"}),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("Unknown rpi docs topic"));
    }

    /// The audit recorded 32 `No matches` responses out of 51 `docs` calls.
    /// These are the reported shapes: long Chinese queries, compound keywords,
    /// and multi-word phrases that never appear verbatim on one line.
    #[test]
    fn review_issue14_compound_queries_are_tokenized() {
        let cases = [
            "rpi dev 开发 循环",
            "extension APIs .rpi configuration",
            "models.json provider config",
            "session resume 分页",
            "RUSTFLAGS -D warnings cargo check",
        ];
        for query in cases {
            let terms = query_terms(query);
            assert!(
                terms.len() >= 2,
                "query must be split: {query} -> {terms:?}"
            );
        }
        assert_eq!(
            query_terms("AgentBuilder"),
            vec!["agentbuilder".to_string()]
        );
        // Punctuation-only separators do not produce empty terms.
        assert!(query_terms("/ , ; : |").is_empty());
    }

    /// AND, not OR: a line containing only one of two terms must not match.
    #[test]
    fn review_issue14_all_terms_must_appear_on_the_same_line() {
        let terms = query_terms("models.json provider");
        assert_eq!(terms, vec!["models.json", "provider"]);
        assert!(line_matches("set models.json provider here", &terms));
        assert!(!line_matches("models.json only", &terms));
        assert!(!line_matches("provider only", &terms));
        assert!(!line_matches("nothing relevant", &terms));
        // Case-insensitive, and CJK runs are kept intact.
        let cjk = query_terms("资源发现 skills");
        assert_eq!(cjk, vec!["资源发现", "skills"]);
        assert!(line_matches("使用 资源发现 和 Skills 目录", &cjk));
    }

    /// A query that misses the selected topic but exists elsewhere must be
    /// answered from that other topic, with the substituted topic named.
    #[tokio::test]
    async fn review_issue14_no_match_falls_back_to_another_topic() {
        // `RPI_FRAME_DIAGNOSTICS` lives in the debugging page; searching for it
        // under `agent` used to return a bare `No matches`.
        let output = execute(serde_json::json!({
            "topic": "agent",
            "query": "RPI_FRAME_DIAGNOSTICS"
        }))
        .await;
        assert!(
            output.contains("No matches for `RPI_FRAME_DIAGNOSTICS` in `agent`"),
            "{output}"
        );
        assert!(output.contains("Found it in `"), "{output}");
        assert!(!output.contains("Found it in `agent`"), "{output}");
        assert!(output.contains("RPI_FRAME_DIAGNOSTICS"), "{output}");
    }

    /// A genuinely absent keyword must still say "No matches" and must not
    /// claim a fallback that never happened.
    #[tokio::test]
    async fn review_issue14_unknown_query_still_reports_no_match() {
        let output = execute(serde_json::json!({
            "topic": "overview",
            "query": "zzz-nonexistent-keyword-zzz"
        }))
        .await;
        assert!(output.contains("No matches for"), "{output}");
        assert!(!output.contains("Found it in"), "{output}");
    }

    /// Single-term and exact-phrase queries keep their previous behaviour: the
    /// existing `rpi dev` / `cdylib` lookups must not regress into AND-only
    /// misses.
    #[tokio::test]
    async fn review_issue14_existing_single_term_queries_still_hit() {
        let output = execute(serde_json::json!({"topic": "debugging", "query": "rpi dev"})).await;
        assert!(output.contains("Matches for `rpi dev`"), "{output}");
        assert!(output.contains("rpi dev"), "{output}");
    }
}
