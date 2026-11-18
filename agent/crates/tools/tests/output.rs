use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agent::agent_tool::AgentTool;
use agent::error::AgentError;
use agent::types::{AgentToolResult, TextContentOrImage};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tools::env::ReadFile;
use tools::{
    create_output_tool, ExecutionEnv, ExecutionError, ExecutionToolContext, FileContent, FileError,
    FileInfo, FileSystem, InMemoryExecutionEnv, Shell, ShellExecOptions, ShellOutput,
};

struct GuardEnv {
    inner: Arc<InMemoryExecutionEnv>,
    supported: bool,
    length_supported: bool,
    max_read: Arc<AtomicUsize>,
}

struct GuardReader {
    inner: Arc<dyn ReadFile>,
    length_supported: bool,
    max_read: Arc<AtomicUsize>,
}

#[async_trait]
impl ReadFile for GuardReader {
    async fn read_all(&self) -> Result<Vec<u8>, FileError> {
        panic!("output must never eagerly read a saved file")
    }

    async fn read_range(&self, offset: u64, length: usize) -> Result<Vec<u8>, FileError> {
        assert!(length <= 64 * 1024, "ranged reads must remain bounded");
        self.max_read.fetch_max(length, Ordering::Relaxed);
        self.inner.read_range(offset, length).await
    }

    async fn byte_len(&self) -> Result<Option<u64>, FileError> {
        if self.length_supported {
            self.inner.byte_len().await
        } else {
            Ok(None)
        }
    }
}

// Only path resolution and stable ranged reads are allowed. In particular,
// neither command execution nor a whole-file fallback can hide in these tests.
#[async_trait]
impl FileSystem for GuardEnv {
    fn cwd(&self) -> &Path {
        self.inner.cwd()
    }
    async fn absolute_path(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        self.inner.absolute_path(path, cancel).await
    }
    async fn open_read_file(&self, path: &str) -> Result<Option<Arc<dyn ReadFile>>, FileError> {
        if !self.supported {
            return Ok(None);
        }
        let inner = self
            .inner
            .open_read_file(path)
            .await?
            .expect("memory reader");
        Ok(Some(Arc::new(GuardReader {
            inner,
            length_supported: self.length_supported,
            max_read: self.max_read.clone(),
        })))
    }
    async fn join_path(
        &self,
        _: &[&str],
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        panic!("unexpected filesystem operation")
    }
    async fn read_text_file(
        &self,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<String, FileError> {
        panic!("eager text read")
    }
    async fn read_text_lines(
        &self,
        _: &str,
        _: Option<usize>,
        _: Option<&CancellationToken>,
    ) -> Result<Vec<String>, FileError> {
        panic!("unexpected text read")
    }
    async fn read_binary_file(
        &self,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<Vec<u8>, FileError> {
        panic!("eager binary read")
    }
    async fn write_file(
        &self,
        _: &str,
        _: FileContent,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        panic!("output must not write")
    }
    async fn append_file(
        &self,
        _: &str,
        _: FileContent,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        panic!("output must not append")
    }
    async fn rename_file(
        &self,
        _: &str,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        panic!("output must not rename")
    }
    async fn file_info(
        &self,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<FileInfo, FileError> {
        panic!("unexpected filesystem operation")
    }
    async fn list_dir(
        &self,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<Vec<FileInfo>, FileError> {
        panic!("unexpected filesystem operation")
    }
    async fn canonical_path(
        &self,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        panic!("unexpected filesystem operation")
    }
    async fn exists(&self, _: &str, _: Option<&CancellationToken>) -> Result<bool, FileError> {
        panic!("unexpected filesystem operation")
    }
    async fn create_dir(
        &self,
        _: &str,
        _: bool,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        panic!("output must not create directories")
    }
    async fn remove(
        &self,
        _: &str,
        _: bool,
        _: bool,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        panic!("output must not remove files")
    }
    async fn create_temp_dir(
        &self,
        _: Option<&str>,
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        panic!("output must not create temporary directories")
    }
    async fn create_temp_file(
        &self,
        _: &str,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        panic!("output must not create temporary files")
    }
    async fn cleanup(&self) {
        panic!("unexpected cleanup")
    }
}

#[async_trait]
impl Shell for GuardEnv {
    async fn exec<'a>(
        &'a self,
        _: &str,
        _: ShellExecOptions<'a>,
    ) -> Result<ShellOutput, ExecutionError> {
        panic!("saved output inspection must never execute commands")
    }
    async fn cleanup(&self) {
        panic!("unexpected shell cleanup")
    }
}
impl ExecutionEnv for GuardEnv {}

async fn fixture(
    bytes: Vec<u8>,
) -> (
    Arc<InMemoryExecutionEnv>,
    Arc<dyn AgentTool>,
    Arc<AtomicUsize>,
) {
    let inner = Arc::new(InMemoryExecutionEnv::with_cwd("/tmp/work".into()));
    let path = inner.absolute_path("saved.log", None).await.unwrap();
    inner.seed_file(&path.to_string_lossy(), bytes).await;
    let max_read = Arc::new(AtomicUsize::new(0));
    let env = Arc::new(GuardEnv {
        inner: inner.clone(),
        supported: true,
        length_supported: true,
        max_read: max_read.clone(),
    });
    let context = ExecutionToolContext::new(env, None);
    (inner, create_output_tool(&context), max_read)
}

async fn run(tool: &Arc<dyn AgentTool>, params: Value) -> AgentToolResult {
    tool.execute(
        "output-test",
        params,
        CancellationToken::new(),
        Arc::new(|_| {}),
    )
    .await
    .unwrap()
}

fn text(result: &AgentToolResult) -> &str {
    match &result.content[0] {
        TextContentOrImage::Text(content) => &content.text,
        _ => panic!("expected text output"),
    }
}

fn continue_params(result: &AgentToolResult, pattern: Option<&str>) -> Value {
    let mut params = json!({
        "path": "saved.log",
        "offset": result.details["next_offset"],
        "byte_offset": result.details["next_byte_offset"],
    });
    if let Some(pattern) = pattern {
        params["pattern"] = json!(pattern);
    }
    params
}

fn source_parts(result: &AgentToolResult) -> Vec<&str> {
    text(result)
        .lines()
        .filter_map(|line| {
            if !line.as_bytes().first().is_some_and(u8::is_ascii_digit) {
                return None;
            }
            let (_, payload) = line.split_once(": ").expect("numbered source line");
            Some(
                payload
                    .strip_suffix(" [clipped; continues below]")
                    .unwrap_or(payload),
            )
        })
        .collect()
}

#[tokio::test]
async fn numbered_ranges_crlf_and_eof_do_not_execute_or_write() {
    let original = b"zero\r\none\r\n\r\nthree\nend".to_vec();
    let (env, tool, _) = fixture(original.clone()).await;
    assert_eq!(tool.schema().name, "output");
    assert_eq!(tool.execution_mode(), agent::ToolExecutionMode::Parallel);
    let first = run(&tool, json!({"path":"saved.log", "offset":2, "limit":2})).await;
    assert!(text(&first).starts_with("2: one\n3: \n"));
    assert_eq!(first.details["returned_lines"], 2);
    assert_eq!(first.details["returned_bytes"], 3);
    assert_eq!(first.details["next_offset"], 4);
    assert_eq!(first.details["eof"], false);
    let last = run(&tool, continue_params(&first, None)).await;
    assert_eq!(text(&last), "4: three\n5: end\n");
    assert_eq!(last.details["eof"], true);
    assert!(last.details["next_offset"].is_null());
    assert_eq!(
        env.read_binary_file("saved.log", None).await.unwrap(),
        original
    );
    let beyond = run(&tool, json!({"path":"saved.log", "offset":99})).await;
    assert_eq!(beyond.details["returned_lines"], 0);
    assert_eq!(beyond.details["eof"], true);
}

#[tokio::test]
async fn regex_and_literal_search_preserve_source_line_continuation() {
    let (_, tool, _) = fixture(b"miss\nMatch 1\nMATCH 2\na.b\naXb\nMatch 3".to_vec()).await;
    let first = run(
        &tool,
        json!({
            "path":"saved.log", "pattern":"^match [0-9]$", "ignore_case":true, "limit":1,
        }),
    )
    .await;
    assert_eq!(source_parts(&first), ["Match 1"]);
    assert_eq!(first.details["next_offset"], 3);
    assert_eq!(first.details["search"], true);
    let second = run(
        &tool,
        json!({
            "path":"saved.log", "pattern":"^match [0-9]$", "ignore_case":true,
            "offset":first.details["next_offset"],
        }),
    )
    .await;
    assert!(text(&second).starts_with("3: MATCH 2\n6: Match 3\n"));
    assert_eq!(second.details["eof"], true);
    let literal = run(
        &tool,
        json!({"path":"saved.log", "pattern":"a.b", "literal":true}),
    )
    .await;
    assert_eq!(text(&literal), "4: a.b\n");
    let none = run(&tool, json!({"path":"saved.log", "pattern":"never"})).await;
    assert_eq!(none.details["returned_lines"], 0);
    assert_eq!(none.details["eof"], true);
}

#[tokio::test]
async fn byte_cap_in_later_result_never_skips_unseen_text() {
    let one = "a".repeat(20_000);
    let two = "b".repeat(40_000);
    let (_, tool, _) = fixture(format!("{one}\n{two}\ntail\n").into_bytes()).await;
    let first = run(&tool, json!({"path":"saved.log"})).await;
    assert!(text(&first).len() <= 50 * 1024);
    assert_eq!(first.details["next_offset"], 2);
    let byte = first.details["next_byte_offset"].as_u64().unwrap();
    assert!(byte > one.len() as u64 + 1);
    let first_parts = source_parts(&first);
    assert_eq!(first_parts[0], one);
    let second = run(&tool, continue_params(&first, None)).await;
    let second_parts = source_parts(&second);
    assert_eq!(format!("{}{}", first_parts[1], second_parts[0]), two);
    assert_eq!(second_parts[1], "tail");
    assert_eq!(second.details["eof"], true);
    assert_eq!(
        first.details["returned_bytes"].as_u64().unwrap()
            + second.details["returned_bytes"].as_u64().unwrap(),
        60_004
    );
}

#[tokio::test]
async fn utf8_crosses_read_chunks_and_oversized_search_continues_the_same_match() {
    // The first UTF-8 code point straddles the 64KiB read boundary. The
    // matching text lies beyond the first rendered page of this source line.
    let body = format!("{}é水tail", "a".repeat(65_535));
    let (_, tool, max_read) = fixture(body.clone().into_bytes()).await;
    let pattern = "é水tail$";
    let first = run(&tool, json!({"path":"saved.log", "pattern":pattern})).await;
    assert_eq!(first.details["next_offset"], 1);
    assert!(first.details["next_byte_offset"].as_u64().unwrap() > 0);
    assert!(text(&first).contains("[clipped; continues below]"));
    assert!(!text(&first).contains('\u{fffd}'));
    let second = run(&tool, continue_params(&first, Some(pattern))).await;
    assert!(!text(&second).contains('\u{fffd}'));
    assert!(text(&first).len() <= 50 * 1024 && text(&second).len() <= 50 * 1024);
    assert_eq!(
        format!("{}{}", source_parts(&first)[0], source_parts(&second)[0]),
        body
    );
    assert_eq!(second.details["eof"], true);
    assert_eq!(max_read.load(Ordering::Relaxed), 64 * 1024);
    assert!(first.details["scanned_bytes"].as_u64().unwrap() >= body.len() as u64);
}

#[tokio::test]
async fn unicode_literals_classes_and_anchors_remain_supported() {
    let (_, tool, _) = fixture("café!\r\ncaféx\r\ncafé\r\nΕλλάδα\r\n".as_bytes().to_vec()).await;
    let anchored = run(&tool, json!({"path":"saved.log", "pattern":r"^caf[é]$"})).await;
    assert_eq!(text(&anchored), "3: café\n");
    let greek = run(
        &tool,
        json!({"path":"saved.log", "pattern":r"^\p{Greek}+$"}),
    )
    .await;
    assert_eq!(text(&greek), "4: Ελλάδα\n");
}

#[tokio::test]
async fn unsupported_unicode_word_boundaries_return_explicit_validation_errors() {
    let (_, tool, _) = fixture("café\n".as_bytes().to_vec()).await;
    let result = tool
        .execute(
            "unsupported-pattern",
            json!({"path":"saved.log", "pattern":r"\bcafé\b"}),
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await;
    match result {
        Err(AgentError::Validation(message)) => {
            assert!(message.contains("unsupported by bounded streaming DFA"));
            assert!(message.contains("4MiB limit"));
        }
        other => panic!("expected explicit unsupported-pattern validation error, got {other:?}"),
    }
}

#[tokio::test]
async fn lossy_utf8_remains_bounded_and_raw_byte_continuation_is_exact() {
    let bytes = vec![0xff; 80_000];
    let (_, tool, _) = fixture(bytes.clone()).await;
    let mut params = json!({"path":"saved.log"});
    let mut returned = 0;
    loop {
        let result = run(&tool, params).await;
        assert!(text(&result).len() <= 50 * 1024);
        returned += result.details["returned_bytes"].as_u64().unwrap();
        if result.details["eof"] == true {
            break;
        }
        assert_eq!(result.details["next_byte_offset"], returned);
        params = continue_params(&result, None);
    }
    assert_eq!(returned, bytes.len() as u64);
}

#[tokio::test]
async fn scanning_can_be_cancelled_after_start_on_an_immediately_ready_backend() {
    let (_, tool, _) = fixture(vec![b'a'; 2 * 1024 * 1024]).await;
    let signal = CancellationToken::new();
    let stop = signal.clone();
    let execution = tool.execute(
        "cancel",
        json!({"path":"saved.log", "pattern":"never"}),
        signal,
        Arc::new(|_| {}),
    );
    let (result, ()) = tokio::join!(execution, async move {
        tokio::task::yield_now().await;
        stop.cancel();
    });
    assert!(result.unwrap_err().to_string().contains("aborted"));
    let signal = CancellationToken::new();
    signal.cancel();
    let result = tool
        .execute(
            "pre-cancel",
            json!({"path":"saved.log"}),
            signal,
            Arc::new(|_| {}),
        )
        .await;
    assert!(result.unwrap_err().to_string().contains("aborted"));
}

#[tokio::test]
async fn unsupported_backends_are_explicit_and_do_not_fallback() {
    for (supported, length_supported) in [(false, true), (true, false)] {
        let inner = Arc::new(InMemoryExecutionEnv::new());
        inner.seed_file("/saved.log", b"data".to_vec()).await;
        let env = Arc::new(GuardEnv {
            inner,
            supported,
            length_supported,
            max_read: Arc::new(AtomicUsize::new(0)),
        });
        let tool = create_output_tool(&ExecutionToolContext::new(env, None));
        let result = tool
            .execute(
                "unsupported",
                json!({"path":"/saved.log"}),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("not_supported"));
    }
}

#[tokio::test]
async fn validates_ranges_and_patterns_and_handles_empty_files() {
    let (_, tool, _) = fixture(Vec::new()).await;
    for params in [
        json!({"path":"saved.log", "offset":0}),
        json!({"path":"saved.log", "limit":0}),
        json!({"path":"saved.log", "limit":2001}),
        json!({"path":"saved.log", "pattern":"["}),
    ] {
        let result = tool
            .execute(
                "invalid",
                params,
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await;
        assert!(matches!(result, Err(AgentError::Validation(_))));
    }
    let result = run(&tool, json!({"path":"saved.log"})).await;
    assert_eq!(result.details["eof"], true);
    assert_eq!(result.details["returned_lines"], 0);
    assert_eq!(result.details["scanned_bytes"], 0);
    assert!(result.details["elapsed_ms"].is_u64());
}

#[tokio::test]
async fn oversized_reads_stop_early_and_preserve_utf8_at_render_boundaries() {
    let body = "水".repeat(400_000);
    let (_, tool, _) = fixture(body.clone().into_bytes()).await;
    let first = run(&tool, json!({"path":"saved.log"})).await;
    assert_eq!(first.details["scanned_bytes"], 64 * 1024);
    let consumed = first.details["returned_bytes"].as_u64().unwrap() as usize;
    assert!(body.is_char_boundary(consumed));
    assert_eq!(source_parts(&first)[0], &body[..consumed]);
    let next = run(&tool, continue_params(&first, None)).await;
    let more = next.details["returned_bytes"].as_u64().unwrap() as usize;
    assert!(body.is_char_boundary(consumed + more));
    assert_eq!(source_parts(&next)[0], &body[consumed..consumed + more]);
    assert!(!text(&next).contains('\u{fffd}'));
}

#[tokio::test]
async fn empty_lines_cannot_overflow_a_nearly_full_page() {
    let mut body = "x".repeat(50_650);
    body.push('\n');
    body.push_str(&"\n".repeat(1999));
    let (_, tool, _) = fixture(body.into_bytes()).await;
    let first = run(&tool, json!({"path":"saved.log", "limit":2000})).await;
    assert!(text(&first).len() <= 50 * 1024);
    assert_eq!(first.details["next_offset"], 2);
    assert!(first.details["next_byte_offset"].is_null());
    let mut params = continue_params(&first, None);
    params["limit"] = json!(2000);
    let rest = run(&tool, params).await;
    assert_eq!(rest.details["returned_lines"], 1999);
    assert_eq!(rest.details["eof"], true);
    assert!(text(&rest).len() <= 50 * 1024);
}
