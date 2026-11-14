//! Mirrors the canonical-path sharing aspect of
//! `packages/agent/test/harness/tools.test.ts` (the "serializes concurrent edits
//! through canonical and symlink paths" + write-queue-locked-until-aborted-settles
//! cases), adapted to `InMemoryExecutionEnv`. The in-memory env does not support
//! real symlinks, so the canonical-path sharing case is expressed via two equal
//! absolute spellings; the abort-bracketing case uses a slow-shell-style env
//! override is covered in `abort_bracketing.rs`. This file focuses on:
//!
//! 1. Two edits to the SAME path issued concurrently must serialize — the final
//!    content reflects both, with no lost update.
//! 2. The mutation queue is keyed by canonical path: an edit via a registered
//!    alias should not race a direct edit. (In-memory: canonical == absolute for
//!    existing files, so we assert the same-path serialization directly.)

use std::sync::Arc;

use rpi_agent::agent_tool::AgentTool;
use rpi_tools::{ExecutionToolContext, FileSystem, InMemoryExecutionEnv};
use tokio_util::sync::CancellationToken;

fn fresh_context() -> (Arc<InMemoryExecutionEnv>, ExecutionToolContext) {
    let env = Arc::new(InMemoryExecutionEnv::with_cwd("/tmp/work".into()));
    let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
    let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
    let ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));
    (env, ctx)
}

async fn seed(env: &InMemoryExecutionEnv, rel: &str, bytes: Vec<u8>) {
    let abs = env.absolute_path(rel, None).await.expect("abs");
    env.seed_file(&abs.to_string_lossy(), bytes).await;
}

async fn read_back(env: &InMemoryExecutionEnv, rel: &str) -> String {
    let abs = env.absolute_path(rel, None).await.expect("abs");
    env.read_text_file(&abs.to_string_lossy(), None)
        .await
        .expect("read")
}

async fn run_edit_raw(
    tool: Arc<dyn AgentTool>,
    params: serde_json::Value,
) -> Result<rpi_agent::types::AgentToolResult, rpi_agent::error::AgentError> {
    let signal = CancellationToken::new();
    let on_update = Arc::new(|_p| ());
    let prepared = tool
        .prepare_arguments(params.clone())
        .unwrap_or_else(|_| params);
    tool.execute("edit-x", prepared, signal, on_update).await
}

#[tokio::test]
async fn concurrent_edits_to_same_path_serialize_no_lost_update() {
    let (env, ctx) = fresh_context();
    // Start with 100 lines so two disjoint edits target different regions.
    let body: String = (0..100)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    seed(&env, "file.txt", body.into_bytes()).await;

    let tool_a = rpi_tools::create_edit_tool(&ctx);
    let tool_b = rpi_tools::create_edit_tool(&ctx);

    let a = tokio::spawn(run_edit_raw(
        tool_a,
        serde_json::json!({ "path": "file.txt", "edits": [{ "oldText": "line0", "newText": "LINE0" }] }),
    ));
    let b = tokio::spawn(run_edit_raw(
        tool_b,
        serde_json::json!({ "path": "file.txt", "edits": [{ "oldText": "line99", "newText": "LINE99" }] }),
    ));
    let (ra, rb) = tokio::join!(a, b);
    ra.unwrap().expect("edit a ok");
    rb.unwrap().expect("edit b ok");

    let after = read_back(&env, "file.txt").await;
    assert!(after.contains("LINE0"), "first edit applied: {after}");
    assert!(after.contains("LINE99"), "second edit applied: {after}");
    assert!(!after.contains("line0\n"));
    assert!(!after.contains("line99"));
}

#[tokio::test]
async fn concurrent_writes_to_same_path_serialize_last_wins() {
    let (env, ctx) = fresh_context();
    let tool_a = rpi_tools::create_write_tool(&ctx);
    let tool_b = rpi_tools::create_write_tool(&ctx);

    let a = tokio::spawn(run_edit_raw(
        tool_a,
        serde_json::json!({ "path": "w.txt", "content": "AAA" }),
    ));
    let b = tokio::spawn(run_edit_raw(
        tool_b,
        serde_json::json!({ "path": "w.txt", "content": "BBB" }),
    ));
    let (ra, rb) = tokio::join!(a, b);
    ra.unwrap().expect("write a ok");
    rb.unwrap().expect("write b ok");

    let after = read_back(&env, "w.txt").await;
    // One of the two wins; never a torn interleave. Both writes are whole
    // payloads, so the content is exactly one of them.
    assert!(
        after == "AAA" || after == "BBB",
        "expected a whole payload, got: {after}"
    );
}
