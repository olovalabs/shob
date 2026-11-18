//! Tests for the `grep` `AgentTool` — mirrors `packages/agent/test/harness/tools.test.ts`
//! read-only search coverage: literal/regex match with `path:line: text` format,
//! `ignore_case`, no-matches, match `limit` cap, `context` lines use `-`
//! separators, long-line truncation, single-file search uses basename, and the
//! 50KB byte-cap footer. All against `InMemoryExecutionEnv`.

use std::sync::Arc;

use agent::agent_tool::AgentTool;
use agent::error::AgentError;
use tokio_util::sync::CancellationToken;
use tools::{ExecutionToolContext, FileSystem, InMemoryExecutionEnv};

/// Build a fresh in-memory tool context rooted at `/tmp/work`.
fn fresh_context() -> (Arc<InMemoryExecutionEnv>, tools::ExecutionToolContext) {
    let env = Arc::new(InMemoryExecutionEnv::with_cwd("/tmp/work".into()));
    let env_dyn: Arc<dyn tools::ExecutionEnv> = env.clone();
    let mut_env: Arc<dyn tools::MutatingEnv> = env.clone();
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

fn text_output(r: &agent::types::AgentToolResult) -> String {
    use agent::types::TextContentOrImage;
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
) -> Result<agent::types::AgentToolResult, AgentError> {
    let signal = CancellationToken::new();
    let on_update = Arc::new(|_p| ());
    tool.execute("grep-1", params, signal, on_update).await
}

#[tokio::test]
async fn grep_pattern_match_format() {
    let (env, ctx) = fresh_context();
    seed(&env, "a.txt", b"foo\nbar\nbaz\n".to_vec()).await;
    seed(&env, "b.txt", b"hello bar world\n".to_vec()).await;

    let tool = tools::create_grep_tool(&ctx, None);
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
    let tool = tools::create_grep_tool(&ctx, None);
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
    let tool = tools::create_grep_tool(&ctx, None);
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
    let tool = tools::create_grep_tool(&ctx, None);
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
    let tool = tools::create_grep_tool(&ctx, None);
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
    let tool = tools::create_grep_tool(&ctx, None);
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
    let tool = tools::create_grep_tool(&ctx, None);
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
    let tool = tools::create_grep_tool(&ctx, None);
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
    let tool = tools::create_grep_tool(&ctx, None);
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
    let tool = tools::create_grep_tool(&ctx, None);
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
    let env = Arc::new(tools::OsExecutionEnv::with_cwd(root.into()));
    let ctx = ExecutionToolContext::new(env, None);
    let tool = tools::create_grep_tool(&ctx, None);

    let result = run_grep(tool.clone(), serde_json::json!({"pattern": "needle"}))
        .await
        .unwrap();
    assert_eq!(
        text_output(&result),
        "z.txt:1: needle\nnested/keep.tmp:1: needle\nnested/deep/a.txt:1: needle"
    );

    let filtered = run_grep(
        tool.clone(),
        serde_json::json!({"pattern": "needle", "mode": "files", "glob": "nested/**"}),
    )
    .await
    .unwrap();
    assert_eq!(
        text_output(&filtered).replace('\\', "/"),
        "nested/keep.tmp\nnested/deep/a.txt"
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
        Arc::new(tools::OsExecutionEnv::with_cwd(dir.path().into())),
        None,
    );
    let (env, memory) = fresh_context();
    seed(&env, "ctx.txt", contents.to_vec()).await;
    for ctx in [native, memory] {
        let result = run_grep(
            tools::create_grep_tool(&ctx, None),
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
        Arc::new(tools::OsExecutionEnv::with_cwd(dir.path().into())),
        None,
    );
    let result = run_grep(
        tools::create_grep_tool(&ctx, None),
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
        Arc::new(tools::OsExecutionEnv::with_cwd(dir.path().into())),
        None,
    );
    let signal = CancellationToken::new();
    signal.cancel();
    let error = tools::create_grep_tool(&ctx, None)
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
        Arc::new(tools::OsExecutionEnv::with_cwd(dir.path().into())),
        None,
    );
    let result = run_grep(
        tools::create_grep_tool(&ctx, None),
        serde_json::json!({"pattern": "needle", "limit": 1000}),
    )
    .await
    .unwrap();
    let out = text_output(&result);
    let body = out.split("\n\n").next().unwrap();
    assert!(body.starts_with("output.txt:1: needle "));
    assert!(body.len() <= tools::DEFAULT_MAX_BYTES);
    assert!(!body.contains("output.txt:200:"));
    assert_eq!(result.details["truncation"]["truncated"], true);
    assert!(result.details.get("match_limit_reached").is_none());
}

/// Keep the directory alive while the native and in-memory tools share fixtures.
async fn search_contexts(
    files: &[(&str, Vec<u8>)],
) -> (tempfile::TempDir, Vec<ExecutionToolContext>) {
    let directory = tempfile::tempdir().unwrap();
    let (memory, memory_context) = fresh_context();
    for (name, bytes) in files {
        let path = directory.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        // seed_file inserts only the file; mirror native parent creation so
        // nested fixtures are reachable through the virtual directory walker.
        let parent = std::path::Path::new(name).parent().unwrap();
        if !parent.as_os_str().is_empty() {
            memory
                .create_dir(&parent.to_string_lossy(), true, None)
                .await
                .unwrap();
        }
        seed(&memory, name, bytes.clone()).await;
    }
    let native_context = ExecutionToolContext::new(
        Arc::new(tools::OsExecutionEnv::with_cwd(directory.path().into())),
        None,
    );
    (directory, vec![native_context, memory_context])
}

fn result_body(result: &agent::types::AgentToolResult) -> Vec<String> {
    text_output(result)
        .split("\n\n")
        .next()
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn grep_all_modes_page_without_dropping_or_repeating_results() {
    let (_directory, contexts) = search_contexts(&[
        (
            "A.txt",
            b"needle one\nother\nneedle two\nneedle three\n".to_vec(),
        ),
        ("a.txt", b"needle four\n".to_vec()),
        ("b:colon.txt", b"needle five\nneedle six\n".to_vec()),
        ("z.txt", b"needle seven\n".to_vec()),
        ("child/first.txt", b"needle eight\n".to_vec()),
    ])
    .await;
    for context in contexts {
        let tool = tools::create_grep_tool(&context, None);
        for mode in ["content", "files", "count"] {
            let complete = run_grep(
                tool.clone(),
                serde_json::json!({"pattern": "needle", "mode": mode}),
            )
            .await
            .unwrap();
            let expected = result_body(&complete);
            assert!(expected[0].starts_with("A.txt"), "{mode}: {expected:?}");
            assert!(expected.last().unwrap().starts_with("child/first.txt"));
            let mut actual = Vec::new();
            let mut offset = 0;
            loop {
                let page = run_grep(
                    tool.clone(),
                    serde_json::json!({
                        "pattern": "needle", "mode": mode, "limit": 2, "offset": offset
                    }),
                )
                .await
                .unwrap();
                let rows = result_body(&page);
                assert!(rows.len() <= 2);
                assert_eq!(page.details["returned_results"], rows.len());
                assert_eq!(page.details["display_bytes"], text_output(&page).len());
                actual.extend(rows);
                match page.details["next_offset"].as_u64() {
                    Some(next) => {
                        assert_eq!(
                            next,
                            offset + page.details["returned_results"].as_u64().unwrap()
                        );
                        assert_eq!(page.details["search_complete"], false);
                        assert_eq!(page.details["totals_exact"], false);
                        offset = next;
                    }
                    None => break,
                }
                assert!(actual.len() < 20, "cursor did not terminate");
            }
            assert_eq!(actual, expected, "{mode}");
        }
    }
}

#[tokio::test]
async fn grep_files_and_count_use_backend_paths_and_count_completed_files() {
    let (_directory, contexts) = search_contexts(&[
        ("a:2:odd.txt", b"needle needle\nneedle\n".repeat(100)),
        ("no.txt", b"other\n".to_vec()),
        ("z.txt", b"needle\n".to_vec()),
    ])
    .await;
    for context in contexts {
        let tool = tools::create_grep_tool(&context, None);
        let files = run_grep(
            tool.clone(),
            serde_json::json!({"pattern": "needle", "mode": "files", "context": 3}),
        )
        .await
        .unwrap();
        assert_eq!(text_output(&files), "a:2:odd.txt\nz.txt");
        assert_eq!(files.details["returned_results"], 2);
        assert_eq!(files.details["scanned_matches"], 2);
        // First-match witnesses must never be advertised as a total line count.
        assert_eq!(files.details["totals_exact"], false);
        let counts = run_grep(
            tool.clone(),
            serde_json::json!({"pattern": "needle", "mode": "count", "limit": 1}),
        )
        .await
        .unwrap();
        assert_eq!(result_body(&counts), vec!["a:2:odd.txt: 200"]);
        assert_eq!(counts.details["counts_complete"], true);
        assert_eq!(counts.details["returned_matches"], 200);
        assert_eq!(counts.details["next_offset"], 1);
        let next = run_grep(
            tool.clone(),
            serde_json::json!({"pattern": "needle", "mode": "count", "limit": 1, "offset": 1}),
        )
        .await
        .unwrap();
        assert_eq!(text_output(&next), "z.txt: 1");
        assert!(next.details.get("next_offset").is_none());
        assert_eq!(next.details["counts_complete"], true);
        assert_eq!(next.details["search_complete"], true);
        assert!(next.details.get("match_limit_reached").is_none());
    }
}

#[tokio::test]
async fn grep_byte_capped_content_continuation_retrieves_every_omitted_line() {
    let line = format!("needle {}\r\n", "x".repeat(440));
    let (_directory, contexts) =
        search_contexts(&[("a:b.txt", line.repeat(400).into_bytes())]).await;
    for context in contexts {
        let tool = tools::create_grep_tool(&context, None);
        let mut actual = Vec::new();
        let mut offset = 0;
        let mut pages = 0;
        loop {
            let page = run_grep(
                tool.clone(),
                serde_json::json!({"pattern": "^needle", "limit": 10000, "offset": offset}),
            )
            .await
            .unwrap();
            assert!(text_output(&page).len() <= tools::DEFAULT_MAX_BYTES);
            let rows = result_body(&page);
            assert!(!rows.is_empty());
            actual.extend(rows);
            pages += 1;
            if let Some(next) = page.details["next_offset"].as_u64() {
                assert_eq!(page.details["truncation"]["truncated"], true);
                assert!(page.details.get("match_limit_reached").is_none());
                assert_eq!(next, actual.len() as u64);
                assert!(next > offset);
                offset = next;
            } else {
                break;
            }
            assert!(pages < 10);
        }
        assert!(pages > 1);
        let expected: Vec<String> = (1..=400)
            .map(|number| format!("a:b.txt:{number}: {}", line.trim_end()))
            .collect();
        assert_eq!(actual, expected);
    }
}

#[tokio::test]
async fn grep_compact_byte_caps_keep_whole_file_results_and_exact_counts() {
    let names: Vec<String> = (0..400)
        .map(|index| format!("{index:03}:{}.txt", "p".repeat(175)))
        .collect();
    let files: Vec<(&str, Vec<u8>)> = names
        .iter()
        .map(|name| (name.as_str(), b"needle\nneedle\n".to_vec()))
        .collect();
    let (_directory, contexts) = search_contexts(&files).await;
    for context in contexts {
        let tool = tools::create_grep_tool(&context, None);
        for mode in ["files", "count"] {
            let mut actual = Vec::new();
            let mut offset = 0;
            let mut pages = 0;
            loop {
                let page = run_grep(
                    tool.clone(),
                    serde_json::json!({
                        "pattern": "needle", "mode": mode, "limit": 10000, "offset": offset
                    }),
                )
                .await
                .unwrap();
                assert!(text_output(&page).len() <= tools::DEFAULT_MAX_BYTES);
                let rows = result_body(&page);
                assert_eq!(page.details["returned_results"], rows.len());
                if mode == "count" {
                    assert_eq!(page.details["counts_complete"], true);
                    assert_eq!(page.details["returned_matches"], (rows.len() * 2) as u64);
                }
                actual.extend(rows);
                pages += 1;
                if let Some(next) = page.details["next_offset"].as_u64() {
                    assert_eq!(page.details["truncation"]["truncated"], true);
                    assert!(page.details.get("match_limit_reached").is_none());
                    assert_eq!(next, actual.len() as u64);
                    assert!(next > offset);
                    offset = next;
                } else {
                    break;
                }
                assert!(pages < 5);
            }
            assert!(pages > 1);
            let expected: Vec<String> = names
                .iter()
                .map(|name| {
                    if mode == "count" {
                        format!("{name}: 2")
                    } else {
                        name.clone()
                    }
                })
                .collect();
            assert_eq!(actual, expected, "{mode}");
        }
    }
}

#[tokio::test]
async fn grep_continuation_consumes_matches_already_displayed_as_trailing_context() {
    let (_directory, contexts) = search_contexts(&[(
        "ctx.txt",
        b"before\nneedle one\nneedle two\nneedle three\nbetween\nneedle four\nafter\n".to_vec(),
    )])
    .await;
    for context in contexts {
        let tool = tools::create_grep_tool(&context, None);
        let page = run_grep(
            tool.clone(),
            serde_json::json!({"pattern": "needle", "limit": 2, "context": 1}),
        )
        .await
        .unwrap();
        assert_eq!(page.details["next_offset"], 3);
        assert_eq!(page.details["returned_matches"], 3);
        let next = run_grep(
            tool.clone(),
            serde_json::json!({"pattern": "needle", "limit": 2, "context": 1, "offset": 3}),
        )
        .await
        .unwrap();
        assert_eq!(
            text_output(&next),
            "ctx.txt-5- between\nctx.txt:6: needle four\nctx.txt-7- after"
        );
        assert!(next.details.get("next_offset").is_none());
        let exhausted = run_grep(
            tool,
            serde_json::json!({"pattern": "needle", "context": 1, "offset": 100}),
        )
        .await
        .unwrap();
        assert_eq!(text_output(&exhausted), "No more matches found");
        assert_eq!(exhausted.details["returned_results"], 0);
    }
}

#[tokio::test]
async fn grep_count_labels_binary_terminated_files_partial() {
    // Keep the NUL beyond ripgrep's initial binary probe, so both backends
    // observe matching lines before binary detection stops the file.
    let mut bytes = b"needle\n".repeat(20_000);
    bytes.extend_from_slice(b"\0needle\n");
    let (_directory, contexts) = search_contexts(&[("late.bin", bytes)]).await;
    for context in contexts {
        let result = run_grep(
            tools::create_grep_tool(&context, None),
            serde_json::json!({"pattern": "needle", "mode": "count"}),
        )
        .await
        .unwrap();
        assert!(text_output(&result).contains("(partial: binary data)"));
        assert_eq!(result.details["counts_complete"], false);
        assert_eq!(result.details["totals_exact"], false);
        assert_eq!(result.details["search_complete"], false);
        assert!(result.details["returned_matches"].as_u64().unwrap() > 0);
    }
}

#[tokio::test]
async fn grep_rejects_invalid_page_bounds_and_unknown_modes() {
    let (_env, context) = fresh_context();
    let tool = tools::create_grep_tool(&context, None);
    for params in [
        serde_json::json!({"pattern": "x", "limit": 0}),
        serde_json::json!({"pattern": "x", "limit": 10001}),
        serde_json::json!({"pattern": "x", "context": 1001}),
        serde_json::json!({"pattern": "x", "offset": u64::MAX}),
        serde_json::json!({"pattern": "x", "offset": -1}),
        serde_json::json!({"pattern": "x", "mode": "unknown"}),
    ] {
        let error = run_grep(tool.clone(), params).await.unwrap_err();
        assert!(matches!(error, AgentError::Validation(_)), "{error:?}");
    }
}

#[tokio::test]
async fn grep_offset_does_not_emit_distant_skipped_match_context() {
    let (_directory, contexts) = search_contexts(&[(
        "gap.txt",
        b"needle old\nold trailing\ngap one\ngap two\nnew leading\nneedle new\nnew trailing\n"
            .to_vec(),
    )])
    .await;
    for context in contexts {
        let result = run_grep(
            tools::create_grep_tool(&context, None),
            serde_json::json!({"pattern": "needle", "context": 1, "offset": 1}),
        )
        .await
        .unwrap();
        assert_eq!(
            text_output(&result),
            "gap.txt-5- new leading\ngap.txt:6: needle new\ngap.txt-7- new trailing"
        );
    }
}
