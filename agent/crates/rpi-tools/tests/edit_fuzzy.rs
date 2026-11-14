//! Mirrors `packages/agent/test/harness/tools.test.ts` "edit" describe block —
//! disjoint edits + both diff formats, overlap rejection, missing/duplicate
//! target rejection, BOM + CRLF preservation, NFKC/smart-quote fuzzy match,
//! and a no-op edit rejection.

use std::sync::Arc;

use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_tools::{ExecutionToolContext, FileSystem, InMemoryExecutionEnv};
use tokio_util::sync::CancellationToken;

/// Build a fresh in-memory tool context rooted at `/tmp/work`.
fn fresh_context() -> (Arc<InMemoryExecutionEnv>, ExecutionToolContext) {
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

async fn read_back(env: &InMemoryExecutionEnv, rel: &str) -> String {
    let abs = env
        .absolute_path(rel, None)
        .await
        .expect("absolute_path for read");
    env.read_text_file(&abs.to_string_lossy(), None)
        .await
        .expect("read_text_file")
}

async fn run_edit(
    tool: Arc<dyn AgentTool>,
    params: serde_json::Value,
) -> Result<rpi_agent::types::AgentToolResult, AgentError> {
    let signal = CancellationToken::new();
    let on_update = Arc::new(|_p| ());
    // prepare_arguments folds legacy fields + JSON-string edits, mirroring the
    // TS pre-validation step the loop applies before execute.
    let prepared = tool
        .prepare_arguments(params.clone())
        .unwrap_or_else(|_| params);
    tool.execute("edit-1", prepared, signal, on_update).await
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

#[tokio::test]
async fn accepts_single_edit_object_and_json_string_object() {
    let (env, ctx) = fresh_context();
    seed(&env, "single.txt", b"alpha\nbeta\n".to_vec()).await;
    let tool = rpi_tools::create_edit_tool(&ctx);

    run_edit(
        tool.clone(),
        serde_json::json!({
            "path": "single.txt",
            "edits": { "oldText": "alpha", "newText": "ALPHA" }
        }),
    )
    .await
    .expect("single edit object should be normalized");
    assert_eq!(read_back(&env, "single.txt").await, "ALPHA\nbeta\n");

    run_edit(
        tool,
        serde_json::json!({
            "path": "single.txt",
            "edits": "{\"oldText\":\"beta\",\"newText\":\"BETA\"}"
        }),
    )
    .await
    .expect("JSON-string edit object should be normalized");
    assert_eq!(read_back(&env, "single.txt").await, "ALPHA\nBETA\n");
}
#[tokio::test]
async fn applies_disjoint_edits_and_returns_diffs() {
    let (env, ctx) = fresh_context();
    let original = "alpha\nbeta\ngamma\ndelta\n";
    seed(&env, "edit.txt", original.as_bytes().to_vec()).await;

    let tool = rpi_tools::create_edit_tool(&ctx);
    let result = run_edit(
        tool,
        serde_json::json!({
            "path": "edit.txt",
            "edits": [
                { "oldText": "alpha\n", "newText": "ALPHA\n" },
                { "oldText": "gamma\n", "newText": "GAMMA\n" },
            ]
        }),
    )
    .await
    .expect("edit ok");

    assert_eq!(
        text_output(&result),
        "Successfully replaced 2 block(s) in edit.txt."
    );
    let diff = result.details["diff"].as_str().expect("diff present");
    assert!(diff.contains("ALPHA"), "{diff}");
    assert!(diff.contains("GAMMA"), "{diff}");
    let patch = result.details["patch"].as_str().expect("patch present");
    assert!(patch.contains("ALPHA"), "patch should mention ALPHA");
    assert_eq!(
        read_back(&env, "edit.txt").await,
        "ALPHA\nbeta\nGAMMA\ndelta\n"
    );
}

#[tokio::test]
async fn rejects_overlapping_edits_and_leaves_file_unchanged() {
    let (env, ctx) = fresh_context();
    seed(&env, "edit.txt", b"one\ntwo\nthree\n".to_vec()).await;
    let tool = rpi_tools::create_edit_tool(&ctx);
    let err = run_edit(
        tool,
        serde_json::json!({
            "path": "edit.txt",
            "edits": [
                { "oldText": "one\ntwo\n", "newText": "ONE\nTWO\n" },
                { "oldText": "two\nthree\n", "newText": "TWO\nTHREE\n" },
            ]
        }),
    )
    .await
    .expect_err("overlap should reject");
    let msg = match err {
        AgentError::Tool(m) => m,
        other => panic!("expected Tool error, got {other:?}"),
    };
    assert!(msg.to_lowercase().contains("overlap"), "got: {msg}");
    assert_eq!(read_back(&env, "edit.txt").await, "one\ntwo\nthree\n");
}

#[tokio::test]
async fn rejects_missing_and_duplicate_target() {
    let (env, ctx) = fresh_context();
    seed(&env, "edit.txt", b"foo foo foo".to_vec()).await;
    let tool = rpi_tools::create_edit_tool(&ctx);

    let err_missing = run_edit(
        tool.clone(),
        serde_json::json!({ "path": "edit.txt", "edits": [{ "oldText": "bar", "newText": "baz" }] }),
    )
    .await
    .expect_err("missing should reject");
    let m1 = match err_missing {
        AgentError::Tool(m) => m,
        other => panic!("expected Tool error, got {other:?}"),
    };
    assert!(m1.contains("Could not find the exact text"), "got: {m1}");

    let err_dup = run_edit(
        tool,
        serde_json::json!({ "path": "edit.txt", "edits": [{ "oldText": "foo", "newText": "bar" }] }),
    )
    .await
    .expect_err("duplicate should reject");
    let m2 = match err_dup {
        AgentError::Tool(m) => m,
        other => panic!("expected Tool error, got {other:?}"),
    };
    assert!(m2.contains("Found 3 occurrences"), "got: {m2}");
}

#[tokio::test]
async fn preserves_bom_and_crlf() {
    let (env, ctx) = fresh_context();
    // BOM + CRLF line endings.
    let body = "\u{FEFF}one\r\ntwo\r\n";
    seed(&env, "edit.txt", body.as_bytes().to_vec()).await;

    let tool = rpi_tools::create_edit_tool(&ctx);
    let _result = run_edit(
        tool,
        serde_json::json!({
            "path": "edit.txt",
            "edits": [{ "oldText": "two", "newText": "TWO" }]
        }),
    )
    .await
    .expect("edit ok");

    assert_eq!(read_back(&env, "edit.txt").await, "\u{FEFF}one\r\nTWO\r\n");
}

#[tokio::test]
async fn fuzzy_matches_smart_quote_apostrophe() {
    // The file uses a right single quote (U+2019); the edit offers an ASCII
    // apostrophe. The fuzzy matcher (NFKC + smart-quote normalization) should
    // still locate the target.
    let (env, ctx) = fresh_context();
    let body = "it\u{2019}s a file\n";
    seed(&env, "edit.txt", body.as_bytes().to_vec()).await;

    let tool = rpi_tools::create_edit_tool(&ctx);
    let result = run_edit(
        tool,
        serde_json::json!({
            "path": "edit.txt",
            "edits": [{ "oldText": "it's a file", "newText": "it is a file" }]
        }),
    )
    .await
    .expect("fuzzy edit ok");
    assert_eq!(
        text_output(&result),
        "Successfully replaced 1 block(s) in edit.txt."
    );
    assert_eq!(read_back(&env, "edit.txt").await, "it is a file\n");
}

#[tokio::test]
async fn rejects_noop_edit() {
    let (env, ctx) = fresh_context();
    seed(&env, "edit.txt", b"alpha\n".to_vec()).await;
    let tool = rpi_tools::create_edit_tool(&ctx);
    let err = run_edit(
        tool,
        serde_json::json!({
            "path": "edit.txt",
            "edits": [{ "oldText": "alpha", "newText": "alpha" }]
        }),
    )
    .await
    .expect_err("no-op should reject");
    let m = match err {
        AgentError::Tool(m) => m,
        other => panic!("expected Tool error, got {other:?}"),
    };
    assert!(
        m.to_lowercase().contains("no change") || m.to_lowercase().contains("no-op"),
        "got: {m}"
    );
}
