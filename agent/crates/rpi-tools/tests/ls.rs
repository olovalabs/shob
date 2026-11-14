//! Tests for the `ls` `AgentTool` — mirrors `packages/agent/test/harness/tools.test.ts`
//! read-only listing coverage: directory suffix, sort order, empty dir, entry
//! limit, and the path-not-found / not-a-directory errors. All against
//! `InMemoryExecutionEnv` (the point of the in-process port).

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

/// Seed a directory at an absolute path (the in-memory env auto-creates parent
/// dirs on `write_file`; for explicit empty dirs use `create_dir`).
async fn make_dir(env: &InMemoryExecutionEnv, rel: &str) {
    env.create_dir(rel, true, None).await.expect("create_dir");
}

/// Seed a file by its env-resolved absolute path string.
async fn seed_file(env: &InMemoryExecutionEnv, rel: &str, bytes: Vec<u8>) {
    let abs = env
        .absolute_path(rel, None)
        .await
        .expect("absolute_path for seed");
    env.seed_file(&abs.to_string_lossy(), bytes).await;
}

/// Extract the single text block's text from a tool result (panics if absent).
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

async fn run_ls(
    tool: Arc<dyn AgentTool>,
    params: serde_json::Value,
) -> Result<rpi_agent::types::AgentToolResult, AgentError> {
    let signal = CancellationToken::new();
    let on_update = Arc::new(|_p| ());
    tool.execute("ls-1", params, signal, on_update).await
}

#[tokio::test]
async fn ls_lists_entries_with_dir_suffix() {
    let (env, ctx) = fresh_context();
    make_dir(&env, "sub").await;
    seed_file(&env, "alpha.txt", b"1".to_vec()).await;
    seed_file(&env, "beta.md", b"2".to_vec()).await;

    let tool = rpi_tools::create_ls_tool(&ctx, None);
    let result = run_ls(tool, serde_json::json!({})).await.expect("ls ok");
    let out = text_output(&result);
    // Directory gets a `/` suffix; files do not.
    assert!(
        out.contains("sub/"),
        "dir should have trailing slash: {out}"
    );
    assert!(out.contains("alpha.txt"), "file alpha present: {out}");
    assert!(out.contains("beta.md"), "file beta present: {out}");
}

#[tokio::test]
async fn ls_sorts_case_insensitively() {
    let (env, ctx) = fresh_context();
    seed_file(&env, "Zebra.txt", b"1".to_vec()).await;
    seed_file(&env, "alpha.txt", b"2".to_vec()).await;
    seed_file(&env, "Beta.txt", b"3".to_vec()).await;

    let tool = rpi_tools::create_ls_tool(&ctx, None);
    let result = run_ls(tool, serde_json::json!({})).await.expect("ls ok");
    let out = text_output(&result);
    let lines: Vec<&str> = out.lines().collect();
    // Case-insensitive order: alpha, Beta, Zebra.
    assert_eq!(lines[0], "alpha.txt");
    assert_eq!(lines[1], "Beta.txt");
    assert_eq!(lines[2], "Zebra.txt");
}

#[tokio::test]
async fn ls_empty_directory() {
    let (env, ctx) = fresh_context();
    make_dir(&env, "empty").await;

    let tool = rpi_tools::create_ls_tool(&ctx, None);
    let result = run_ls(tool, serde_json::json!({ "path": "empty" }))
        .await
        .expect("ls ok");
    assert_eq!(text_output(&result), "(empty directory)");
}

#[tokio::test]
async fn ls_entry_limit_reached() {
    let (env, ctx) = fresh_context();
    // Seed 6 files, limit to 3.
    for i in 0..6 {
        seed_file(&env, &format!("f{i}.txt"), b"x".to_vec()).await;
    }
    let tool = rpi_tools::create_ls_tool(&ctx, None);
    let result = run_ls(tool, serde_json::json!({ "limit": 3 }))
        .await
        .expect("ls ok");
    let out = text_output(&result);
    assert!(out.contains("3 entries limit reached"), "got: {out}");
    assert!(result.details.get("entry_limit_reached").is_some());
    assert_eq!(result.details["entry_limit_reached"], 3);
}

#[tokio::test]
async fn ls_path_not_found() {
    let (_env, ctx) = fresh_context();
    let tool = rpi_tools::create_ls_tool(&ctx, None);
    let err = run_ls(tool, serde_json::json!({ "path": "nope" }))
        .await
        .expect_err("missing path should error");
    let msg = match err {
        AgentError::Tool(m) => m,
        other => panic!("expected Tool error, got {other:?}"),
    };
    assert!(msg.contains("Path not found"), "got: {msg}");
}

#[tokio::test]
async fn ls_not_a_directory() {
    let (env, ctx) = fresh_context();
    seed_file(&env, "afile.txt", b"hi".to_vec()).await;
    let tool = rpi_tools::create_ls_tool(&ctx, None);
    let err = run_ls(tool, serde_json::json!({ "path": "afile.txt" }))
        .await
        .expect_err("file not dir should error");
    let msg = match err {
        AgentError::Tool(m) => m,
        other => panic!("expected Tool error, got {other:?}"),
    };
    assert!(msg.contains("Not a directory"), "got: {msg}");
}
