//! Tests for the `grep` `AgentTool` — mirrors `packages/agent/test/harness/tools.test.ts`
//! read-only search coverage: literal/regex match with `path:line: text` format,
//! `ignore_case`, no-matches, match `limit` cap, `context` lines use `-`
//! separators, long-line truncation, single-file search uses basename, and the
//! 50KB byte-cap footer. All against `InMemoryExecutionEnv`.

use std::sync::Arc;

use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_tools::{ExecutionToolContext, FileSystem, InMemoryExecutionEnv};
use tokio_util::sync::CancellationToken;

/// Build a fresh in-memory tool context rooted at `/tmp/work`.
fn fresh_context() -> (Arc<InMemoryExecutionEnv>, rpi_tools::ExecutionToolContext) {
    let env = Arc::new(InMemoryExecutionEnv::with_cwd("/tmp/work".into()));
    let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
    let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
    let ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));
    (env, ctx)
}

async fn seed(env: &InMemoryExecutionEnv, rel: &str, bytes: Vec<u8>) {
    let abs = env
        .absolute_path(rel, None)
        .await
        .expect("absolute_path for seed");
    env.seed_file(&abs.to_string_lossy(), bytes).await;
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

async fn run_grep(
    tool: Arc<dyn AgentTool>,
    params: serde_json::Value,
) -> Result<rpi_agent::types::AgentToolResult, AgentError> {
    let signal = CancellationToken::new();
    let on_update = Arc::new(|_p| ());
    tool.execute("grep-1", params, signal, on_update).await
}

#[tokio::test]
async fn grep_pattern_match_format() {
    let (env, ctx) = fresh_context();
    seed(&env, "a.txt", b"foo\nbar\nbaz\n".to_vec()).await;
    seed(&env, "b.txt", b"hello bar world\n".to_vec()).await;

    let tool = rpi_tools::create_grep_tool(&ctx, None);
    let result = run_grep(tool, serde_json::json!({ "pattern": "bar", "path": "." }))
        .await
        .expect("grep ok");
    let out = text_output(&result);
    // Each match is `relpath:line: text`.
    assert!(out.contains("a.txt:2: bar"), "got: {out}");
    assert!(out.contains("b.txt:1: hello bar world"), "got: {out}");
}

#[tokio::test]
async fn grep_regex_match() {
    let (env, ctx) = fresh_context();
    seed(&env, "nums.txt", b"val=123\nval=456\nother\n".to_vec()).await;
    let tool = rpi_tools::create_grep_tool(&ctx, None);
    let result = run_grep(
        tool,
        serde_json::json!({ "pattern": "val=\\d+", "path": "nums.txt" }),
    )
    .await
    .expect("grep ok");
    let out = text_output(&result);
    assert!(out.contains("nums.txt:1: val=123"), "got: {out}");
    assert!(out.contains("nums.txt:2: val=456"), "got: {out}");
    assert!(!out.contains("other"));
}

#[tokio::test]
async fn grep_literal_escapes_regex() {
    let (env, ctx) = fresh_context();
    seed(&env, "lit.txt", b"1+1=2\n2+2=4\n".to_vec()).await;
    let tool = rpi_tools::create_grep_tool(&ctx, None);
    // Literal `1+1` — the `+` must not act as a regex quantifier.
    let result = run_grep(
        tool,
        serde_json::json!({ "pattern": "1+1", "path": "lit.txt", "literal": true }),
    )
    .await
    .expect("grep ok");
    let out = text_output(&result);
    assert!(out.contains("lit.txt:1: 1+1=2"), "got: {out}");
    assert!(!out.contains("lit.txt:2"));
}

#[tokio::test]
async fn grep_ignore_case() {
    let (env, ctx) = fresh_context();
    seed(&env, "c.txt", b"Hello\nHELLO\nworld\n".to_vec()).await;
    let tool = rpi_tools::create_grep_tool(&ctx, None);
    let result = run_grep(
        tool,
        serde_json::json!({ "pattern": "hello", "path": "c.txt", "ignoreCase": true }),
    )
    .await
    .expect("grep ok");
    let out = text_output(&result);
    assert!(out.contains("c.txt:1: Hello"), "got: {out}");
    assert!(out.contains("c.txt:2: HELLO"), "got: {out}");
    assert!(!out.contains("world"));
}

#[tokio::test]
async fn grep_no_matches() {
    let (env, ctx) = fresh_context();
    seed(&env, "x.txt", b"alpha\nbeta\n".to_vec()).await;
    let tool = rpi_tools::create_grep_tool(&ctx, None);
    let result = run_grep(
        tool,
        serde_json::json!({ "pattern": "zzz", "path": "x.txt" }),
    )
    .await
    .expect("grep ok");
    assert_eq!(text_output(&result), "No matches found");
}

#[tokio::test]
async fn grep_match_limit() {
    let (env, ctx) = fresh_context();
    // 5 match lines; limit to 2.
    let body: String = (0..5).map(|_| "match").collect::<Vec<_>>().join("\n") + "\n";
    seed(&env, "many.txt", body.into_bytes()).await;
    let tool = rpi_tools::create_grep_tool(&ctx, None);
    let result = run_grep(
        tool,
        serde_json::json!({ "pattern": "match", "path": "many.txt", "limit": 2 }),
    )
    .await
    .expect("grep ok");
    let out = text_output(&result);
    assert!(out.contains("2 matches limit reached"), "got: {out}");
    assert!(result.details.get("match_limit_reached").is_some());
    assert_eq!(result.details["match_limit_reached"], 2);
    // Exactly 2 match lines emitted.
    let match_lines = out.lines().filter(|l| l.contains(": match")).count();
    assert_eq!(match_lines, 2);
}

#[tokio::test]
async fn grep_context_lines_use_dash_separator() {
    let (env, ctx) = fresh_context();
    seed(&env, "ctx.txt", b"l1\nl2 MATCH\nl3\nl4\n".to_vec()).await;
    let tool = rpi_tools::create_grep_tool(&ctx, None);
    let result = run_grep(
        tool,
        serde_json::json!({ "pattern": "MATCH", "path": "ctx.txt", "context": 1 }),
    )
    .await
    .expect("grep ok");
    let out = text_output(&result);
    // Context before: `ctx.txt-1- l1`; match: `ctx.txt:2: l2 MATCH`; after: `ctx.txt-3- l3`.
    assert!(out.contains("ctx.txt-1- l1"), "got: {out}");
    assert!(out.contains("ctx.txt:2: l2 MATCH"), "got: {out}");
    assert!(out.contains("ctx.txt-3- l3"), "got: {out}");
}

#[tokio::test]
async fn grep_truncates_long_line() {
    let (env, ctx) = fresh_context();
    // A single line longer than GREP_MAX_LINE_LENGTH (500).
    let long_line: String = "x".repeat(600);
    let body = format!("nomatch\n{long_line}\n");
    seed(&env, "long.txt", body.into_bytes()).await;
    let tool = rpi_tools::create_grep_tool(&ctx, None);
    let result = run_grep(
        tool,
        serde_json::json!({ "pattern": "x", "path": "long.txt" }),
    )
    .await
    .expect("grep ok");
    let out = text_output(&result);
    assert!(out.contains("... [truncated]"), "got: {out}");
    assert_eq!(result.details["lines_truncated"], true);
}

#[tokio::test]
async fn grep_invalid_regex_errors() {
    let (env, ctx) = fresh_context();
    seed(&env, "e.txt", b"x\n".to_vec()).await;
    let tool = rpi_tools::create_grep_tool(&ctx, None);
    let err = run_grep(
        tool,
        serde_json::json!({ "pattern": "(unclosed", "path": "e.txt" }),
    )
    .await
    .expect_err("bad regex should fail");
    assert!(matches!(err, AgentError::Validation(_)), "got: {err:?}");
}

#[tokio::test]
async fn grep_glob_filter() {
    let (env, ctx) = fresh_context();
    seed(&env, "keep.ts", b"target\ntext\n".to_vec()).await;
    seed(&env, "skip.txt", b"target\n".to_vec()).await;
    let tool = rpi_tools::create_grep_tool(&ctx, None);
    // `*.ts` basename glob → only keep.ts is searched.
    let result = run_grep(
        tool,
        serde_json::json!({ "pattern": "target", "path": ".", "glob": "*.ts" }),
    )
    .await
    .expect("grep ok");
    let out = text_output(&result);
    assert!(out.contains("keep.ts"), "got: {out}");
    assert!(
        !out.contains("skip.txt"),
        "txt should be filtered out: {out}"
    );
}

#[tokio::test]
async fn grep_native_walk_respects_ignore_rules_and_preserves_breadth_first_order() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for subdir in [".git", "nested/deep", "target"] {
        std::fs::create_dir_all(root.join(subdir)).unwrap();
    }
    for (name, contents) in [
        (".gitignore", "/target/\n*.tmp\n"),
        ("nested/.gitignore", "!keep.tmp\n"),
        ("z.txt", "needle\r\nother\n"),
        ("nested/keep.tmp", "needle\n"),
        ("nested/skip.tmp", "needle\n"),
        ("nested/deep/a.txt", "needle\n"),
        ("target/output.txt", "needle\n"),
    ] {
        std::fs::write(root.join(name), contents).unwrap();
    }
    let env = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(root.into()));
    let ctx = ExecutionToolContext::new(env, None);
    let tool = rpi_tools::create_grep_tool(&ctx, None);

    let result = run_grep(tool.clone(), serde_json::json!({"pattern": "needle"}))
        .await
        .unwrap();
    assert_eq!(
        text_output(&result),
        "z.txt:1: needle\nnested/keep.tmp:1: needle\nnested/deep/a.txt:1: needle"
    );

    let explicit = run_grep(
        tool,
        serde_json::json!({"pattern": "needle", "path": "target/output.txt"}),
    )
    .await
    .unwrap();
    assert_eq!(text_output(&explicit), "output.txt:1: needle");
}

#[tokio::test]
async fn grep_context_limit_keeps_trailing_lines_without_duplicate_windows() {
    let contents = b"before\nneedle one\nneedle two\nneedle three\nafter\n";
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ctx.txt"), contents).unwrap();
    let native = ExecutionToolContext::new(
        Arc::new(rpi_tools::OsExecutionEnv::with_cwd(dir.path().into())),
        None,
    );
    let (env, memory) = fresh_context();
    seed(&env, "ctx.txt", contents.to_vec()).await;
    for ctx in [native, memory] {
        let result = run_grep(
            rpi_tools::create_grep_tool(&ctx, None),
            serde_json::json!({"pattern": "needle", "context": 1, "limit": 2}),
        )
        .await
        .unwrap();
        assert_eq!(
            text_output(&result).split("\n\n").next().unwrap(),
            "ctx.txt-1- before\nctx.txt:2: needle one\nctx.txt:3: needle two\nctx.txt-4- needle three"
        );
        assert_eq!(result.details["match_limit_reached"], 2);
    }
}

#[tokio::test]
async fn grep_native_handles_crlf_anchors_and_skips_binary_data() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("text.txt"), b"other\r\nNEEDLE\r\nend\r\n").unwrap();
    std::fs::write(dir.path().join("binary.bin"), b"\0header\nNEEDLE\n").unwrap();
    let ctx = ExecutionToolContext::new(
        Arc::new(rpi_tools::OsExecutionEnv::with_cwd(dir.path().into())),
        None,
    );
    let result = run_grep(
        rpi_tools::create_grep_tool(&ctx, None),
        serde_json::json!({"pattern": "^needle$", "ignoreCase": true}),
    )
    .await
    .unwrap();
    assert_eq!(text_output(&result), "text.txt:2: NEEDLE");
}

#[tokio::test]
async fn grep_cancelled_native_search_returns_an_error_not_empty_results() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("text.txt"), b"needle\n").unwrap();
    let ctx = ExecutionToolContext::new(
        Arc::new(rpi_tools::OsExecutionEnv::with_cwd(dir.path().into())),
        None,
    );
    let signal = CancellationToken::new();
    signal.cancel();
    let error = rpi_tools::create_grep_tool(&ctx, None)
        .execute(
            "cancelled",
            serde_json::json!({"pattern": "needle"}),
            signal,
            Arc::new(|_| ()),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("aborted"), "{error}");
}

#[tokio::test]
async fn grep_native_caps_output_bytes_even_with_a_large_match_limit() {
    let dir = tempfile::tempdir().unwrap();
    let line = format!("needle {}\n", "x".repeat(440));
    std::fs::write(dir.path().join("output.txt"), line.repeat(200)).unwrap();
    let ctx = ExecutionToolContext::new(
        Arc::new(rpi_tools::OsExecutionEnv::with_cwd(dir.path().into())),
        None,
    );
    let result = run_grep(
        rpi_tools::create_grep_tool(&ctx, None),
        serde_json::json!({"pattern": "needle", "limit": 1000}),
    )
    .await
    .unwrap();
    let out = text_output(&result);
    let body = out.split("\n\n").next().unwrap();
    assert!(body.starts_with("output.txt:1: needle "));
    assert!(body.len() <= rpi_tools::DEFAULT_MAX_BYTES);
    assert!(!body.contains("output.txt:200:"));
    assert_eq!(result.details["truncation"]["truncated"], true);
    assert!(result.details.get("match_limit_reached").is_none());
}
