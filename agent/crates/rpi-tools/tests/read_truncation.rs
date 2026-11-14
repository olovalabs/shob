//! Mirrors `packages/agent/test/harness/tools.test.ts` "read" describe block —
//! exercises the `read` AgentTool: offset/limit + continuation notice, line-count
//! truncation, the exact-2000-line boundary (trailing newline not over-counted),
//! offset-beyond-end rejection, and image signature detection.

use std::sync::Arc;

use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_tools::{
    ExecutionToolContext, FileSystem, InMemoryExecutionEnv, ProcessedImage, ReadImageProcessor,
    ReadToolOptions,
};
use tokio_util::sync::CancellationToken;

/// Build a fresh in-memory tool context rooted at `/tmp/work`.
fn fresh_context() -> (Arc<InMemoryExecutionEnv>, rpi_tools::ExecutionToolContext) {
    let env = Arc::new(InMemoryExecutionEnv::with_cwd("/tmp/work".into()));
    let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
    let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
    let ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));
    (env, ctx)
}

/// Seed a file by its env-resolved absolute path string (so the storage key the
/// tool resolves to matches exactly — important on Windows where `cwd.join`
/// normalizes separators).
async fn seed(env: &InMemoryExecutionEnv, rel: &str, bytes: Vec<u8>) {
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

async fn run_read(
    tool: Arc<dyn AgentTool>,
    params: serde_json::Value,
) -> Result<rpi_agent::types::AgentToolResult, AgentError> {
    let signal = CancellationToken::new();
    let on_update = Arc::new(|_p| ());
    tool.execute("read-1", params, signal, on_update).await
}

#[tokio::test]
async fn read_replaces_invalid_utf8_like_text_decoder() {
    let (env, ctx) = fresh_context();
    seed(&env, "invalid.txt", vec![b'a', 0xff, b'b']).await;

    let tool = rpi_tools::create_read_tool(&ctx, None);
    let result = run_read(tool, serde_json::json!({ "path": "invalid.txt" }))
        .await
        .expect("invalid UTF-8 should be lossily decoded");
    let out = text_output(&result);
    assert_eq!(out, "a\u{fffd}b");
}
struct TestImageProcessor;

impl ReadImageProcessor for TestImageProcessor {
    fn process(
        &self,
        bytes: &[u8],
        mime_type: &str,
        auto_resize: bool,
    ) -> Result<ProcessedImage, String> {
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        Ok(ProcessedImage {
            data: "processed".to_string(),
            mime_type: mime_type.to_string(),
            hints: vec![format!("resize={auto_resize}")],
        })
    }
}

#[tokio::test]
async fn read_uses_injected_image_processor_and_resize_option() {
    let (env, ctx) = fresh_context();
    seed(&env, "image.png", base64_png()).await;

    let tool = rpi_tools::create_read_tool(
        &ctx,
        Some(ReadToolOptions {
            auto_resize_images: false,
            image_processor: Some(Arc::new(TestImageProcessor)),
        }),
    );
    let result = run_read(tool, serde_json::json!({ "path": "image.png" }))
        .await
        .expect("processor path should succeed");
    assert!(text_output(&result).contains("resize=false"));
    assert!(matches!(
        result.content.last(),
        Some(rpi_agent::types::TextContentOrImage::Image(image)) if &*image.data == "processed"
    ));
}
#[tokio::test]
async fn read_offsets_limits_and_continuation_notice() {
    let (env, ctx) = fresh_context();
    let body: String = (0..100)
        .map(|i| format!("Line {}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    seed(&env, "test.txt", body.into_bytes()).await;

    let tool = rpi_tools::create_read_tool(&ctx, None);
    let result = run_read(
        tool,
        serde_json::json!({ "path": "test.txt", "offset": 41, "limit": 20 }),
    )
    .await
    .expect("read ok");
    let out = text_output(&result);

    assert!(!out.contains("Line 40"));
    assert!(out.contains("Line 41"));
    assert!(out.contains("Line 60"));
    assert!(!out.contains("Line 61"));
    assert!(out.contains("[40 more lines in file. Use offset=61 to continue.]"));
}

#[tokio::test]
async fn read_truncates_large_text_by_line_count() {
    let (env, ctx) = fresh_context();
    let body: String = (0..2500)
        .map(|i| format!("Line {}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    seed(&env, "large.txt", body.into_bytes()).await;

    let tool = rpi_tools::create_read_tool(&ctx, None);
    let result = run_read(tool, serde_json::json!({ "path": "large.txt" }))
        .await
        .expect("read ok");
    assert!(text_output(&result)
        .contains("[Showing lines 1-2000 of 2500. Use offset=2001 to continue.]"));
    // details.truncation snapshot.
    let trunc = result
        .details
        .get("truncation")
        .expect("truncation present");
    assert_eq!(trunc["truncated"], true);
    assert_eq!(trunc["truncated_by"], "lines");
    assert_eq!(trunc["total_lines"], 2500);
    assert_eq!(trunc["output_lines"], 2000);
}

#[tokio::test]
async fn read_does_not_overcount_trailing_newline_at_limit() {
    let (env, ctx) = fresh_context();
    // Exactly 2000 "x" lines joined by '\n' + a trailing '\n'.
    let body: String = (0..2000).map(|_| "x").collect::<Vec<_>>().join("\n") + "\n";
    seed(&env, "exact.txt", body.into_bytes()).await;

    let tool = rpi_tools::create_read_tool(&ctx, None);
    let result = run_read(tool, serde_json::json!({ "path": "exact.txt" }))
        .await
        .expect("read ok");
    // No truncation → details is Null, no continuation notice.
    assert!(result.details.is_null());
    assert!(!text_output(&result).contains("Use offset="));
}

#[tokio::test]
async fn read_rejects_offset_beyond_end() {
    let (env, ctx) = fresh_context();
    seed(&env, "short.txt", b"one\ntwo\nthree".to_vec()).await;
    let tool = rpi_tools::create_read_tool(&ctx, None);
    let err = run_read(
        tool,
        serde_json::json!({ "path": "short.txt", "offset": 100 }),
    )
    .await
    .expect_err("offset beyond end rejects");
    let msg = match err {
        AgentError::Tool(m) => m,
        other => panic!("expected Tool error, got {other:?}"),
    };
    assert!(
        msg.contains("Offset 100 is beyond end of file (3 lines total)"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn read_detects_supported_image_by_content() {
    use rpi_tools::encode_base64;
    let (env, ctx) = fresh_context();
    // Minimal valid 1x1 PNG.
    let png: Vec<u8> = base64_png();
    seed(&env, "image.txt", png.clone()).await;

    let tool = rpi_tools::create_read_tool(&ctx, None);
    let result = run_read(tool, serde_json::json!({ "path": "image.txt" }))
        .await
        .expect("read ok");
    assert!(text_output(&result).contains("Read image file [image/png]"));
    // The image content block carries the base64 of the raw bytes.
    let has_image = result
        .content
        .iter()
        .any(|c| matches!(c, rpi_agent::types::TextContentOrImage::Image(_)));
    assert!(has_image, "expected an image content block");
    let b64 = encode_base64(&png);
    let found = result
        .content
        .iter()
        .any(|c| matches!(c, rpi_agent::types::TextContentOrImage::Image(i) if *i.data == *b64));
    assert!(found, "image block data must equal base64(raw bytes)");
}

/// A minimal valid 1×1 PNG (the same bytes the TS test decodes from base64).
fn base64_png() -> Vec<u8> {
    // PNG header + IHDR + IDAT + IEND for a 1x1 transparent pixel.
    #[rustfmt::skip]
    let bytes: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A,
    ];
    // The detector only needs the 8-byte signature; a full valid PNG would
    // require the IHDR/IDAT/CRC dance. detect_supported_image_mime_type keys on
    // the signature, so this suffices — but to be safe, prepend a real PNG.
    // Use the well-known 1x1 PNG base64:
    let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAX+XDSwAAAABJRU5ErkJggg==";
    use base64::Engine;
    let full = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .unwrap();
    let _ = bytes;
    full
}
