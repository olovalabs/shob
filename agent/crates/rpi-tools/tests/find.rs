//! Tests for the `find` `AgentTool` — mirrors `packages/agent/test/harness/tools.test.ts`
//! read-only file-search coverage: basename glob, path glob, no-matches, result
//! `limit` cap, `.git/` skipping, and relativization to posix separators. All
//! against `InMemoryExecutionEnv`.

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

async fn make_dir(env: &InMemoryExecutionEnv, rel: &str) {
    env.create_dir(rel, true, None).await.expect("create_dir");
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

async fn run_find(
    tool: Arc<dyn AgentTool>,
    params: serde_json::Value,
) -> Result<rpi_agent::types::AgentToolResult, AgentError> {
    let signal = CancellationToken::new();
    let on_update = Arc::new(|_p| ());
    tool.execute("find-1", params, signal, on_update).await
}

#[tokio::test]
async fn find_basename_glob() {
    let (env, ctx) = fresh_context();
    seed(&env, "a.rs", b"".to_vec()).await;
    seed(&env, "b.txt", b"".to_vec()).await;
    seed(&env, "c.rs", b"".to_vec()).await;

    let tool = rpi_tools::create_find_tool(&ctx, None);
    let result = run_find(tool, serde_json::json!({ "pattern": "*.rs", "path": "." }))
        .await
        .expect("find ok");
    let out = text_output(&result);
    assert!(out.contains("a.rs"), "got: {out}");
    assert!(out.contains("c.rs"), "got: {out}");
    assert!(!out.contains("b.txt"), "txt should be filtered out: {out}");
}

#[tokio::test]
async fn find_path_glob_recursive() {
    let (env, ctx) = fresh_context();
    make_dir(&env, "src").await;
    seed(&env, "src/config.json", b"{}".to_vec()).await;
    seed(&env, "src/other.txt", b"".to_vec()).await;
    seed(&env, "top.json", b"{}".to_vec()).await;

    let tool = rpi_tools::create_find_tool(&ctx, None);
    let result = run_find(
        tool,
        serde_json::json!({ "pattern": "**/*.json", "path": "." }),
    )
    .await
    .expect("find ok");
    let out = text_output(&result);
    assert!(out.contains("src/config.json"), "got: {out}");
    // `**/*.json` should also match top-level `top.json` (the `**/` prepend
    // makes it match at any depth including depth 0).
    assert!(out.contains("top.json"), "got: {out}");
    assert!(!out.contains("other.txt"), "got: {out}");
}

#[tokio::test]
async fn find_no_matches() {
    let (env, ctx) = fresh_context();
    seed(&env, "a.rs", b"".to_vec()).await;
    let tool = rpi_tools::create_find_tool(&ctx, None);
    let result = run_find(tool, serde_json::json!({ "pattern": "*.foo", "path": "." }))
        .await
        .expect("find ok");
    assert_eq!(text_output(&result), "No files found matching pattern");
}

#[tokio::test]
async fn find_result_limit() {
    let (env, ctx) = fresh_context();
    // Seed 6 files, limit to 3.
    for i in 0..6 {
        seed(&env, &format!("f{i}.rs"), b"".to_vec()).await;
    }
    let tool = rpi_tools::create_find_tool(&ctx, None);
    let result = run_find(
        tool,
        serde_json::json!({ "pattern": "*.rs", "path": ".", "limit": 3 }),
    )
    .await
    .expect("find ok");
    let out = text_output(&result);
    assert!(out.contains("3 results limit reached"), "got: {out}");
    assert!(result.details.get("result_limit_reached").is_some());
    assert_eq!(result.details["result_limit_reached"], 3);
    // Exactly 3 result lines.
    let count = out.lines().filter(|l| l.ends_with(".rs")).count();
    assert_eq!(count, 3);
}

#[tokio::test]
async fn find_skips_git_directory() {
    let (env, ctx) = fresh_context();
    make_dir(&env, ".git").await;
    seed(&env, ".git/HEAD", b"ref".to_vec()).await;
    seed(&env, "real.rs", b"".to_vec()).await;

    let tool = rpi_tools::create_find_tool(&ctx, None);
    // `*` matches everything — but `.git/HEAD` must be excluded.
    let result = run_find(tool, serde_json::json!({ "pattern": "**/*", "path": "." }))
        .await
        .expect("find ok");
    let out = text_output(&result);
    assert!(out.contains("real.rs"), "got: {out}");
    assert!(!out.contains(".git"), ".git should be skipped: {out}");
}

#[tokio::test]
async fn find_path_not_found() {
    let (_env, ctx) = fresh_context();
    let tool = rpi_tools::create_find_tool(&ctx, None);
    let err = run_find(
        tool,
        serde_json::json!({ "pattern": "*.rs", "path": "does-not-exist" }),
    )
    .await
    .expect_err("missing path should error");
    let msg = match err {
        AgentError::Tool(m) => m,
        other => panic!("expected Tool error, got {other:?}"),
    };
    assert!(msg.contains("Path not found"), "got: {msg}");
}

#[tokio::test]
async fn find_directory_match_has_trailing_slash() {
    let (env, ctx) = fresh_context();
    make_dir(&env, "components").await;
    seed(&env, "components/button.rs", b"".to_vec()).await;

    let tool = rpi_tools::create_find_tool(&ctx, None);
    // `components` matches the dir glob — should carry a trailing `/`.
    let result = run_find(
        tool,
        serde_json::json!({ "pattern": "components", "path": "." }),
    )
    .await
    .expect("find ok");
    let out = text_output(&result);
    assert!(
        out.contains("components/"),
        "dir match should have trailing slash: {out}"
    );
}
