//! Source fixtures only for the neutral client seam; no transport/CLI behavior
//! is implied by these tests. Host calls panic intentionally in this test double.
use super::*;
use crate::env::{
    ExecutionEnv, FileContent, FileInfo, FileSystem, Shell, ShellExecOptions, ShellOutput,
};
use crate::error::ExecutionError;
use crate::file_mutation_queue::{MutatingEnv, MutationQueueRegistry};
use crate::{
    create_edit_tool, create_read_tool, create_write_tool, ExecutionToolContext,
    InMemoryExecutionEnv,
};
use rpi_agent::agent_tool::AgentTool;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

#[derive(Default)]
struct Backend {
    text: Mutex<String>,
    requests: Mutex<Vec<FileIORequest>>,
    error: Mutex<Option<FileIOErrorCode>>,
    response: Mutex<Option<FileIOResponse>>,
    cancel_after: AtomicBool,
    hold_write: AtomicBool,
    entered: Notify,
    release: Notify,
}

impl Backend {
    fn text(text: &str) -> Arc<Self> {
        Arc::new(Self {
            text: Mutex::new(text.to_owned()),
            ..Default::default()
        })
    }
}

#[async_trait]
impl FileIOBackend for Backend {
    async fn request(
        &self,
        request: FileIORequest,
        cancel: CancellationToken,
    ) -> Result<FileIOResponse, FileIOError> {
        self.requests.lock().await.push(request.clone());
        if self.cancel_after.load(Ordering::SeqCst) {
            cancel.cancel();
        }
        if let Some(code) = *self.error.lock().await {
            return Err(FileIOError::new(code, "fixture failure"));
        }
        if let Some(response) = self.response.lock().await.clone() {
            return Ok(response);
        }
        match request {
            FileIORequest::ReadText { range, .. } => {
                let text = self.text.lock().await;
                let content = match range {
                    FileIOReadRange::Complete => text.clone(),
                    FileIOReadRange::Lines {
                        offset_1based,
                        limit,
                    } => {
                        let lines = crate::truncate::split_lines_for_counting(&text)
                            .into_iter()
                            .skip(offset_1based.saturating_sub(1) as usize)
                            .take(limit as usize)
                            .collect::<Vec<_>>();
                        let mut result = lines.join("\n");
                        if lines.last() == Some(&"") {
                            result.push('\n');
                        }
                        result
                    }
                };
                Ok(FileIOResponse::Text { content })
            }
            FileIORequest::WriteText { content, .. } => {
                if self.hold_write.swap(false, Ordering::SeqCst) {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                *self.text.lock().await = content;
                Ok(FileIOResponse::Written)
            }
        }
    }
}

// Implement the actual required traits, not an imagined alternate env API.
struct TrapEnv {
    cwd: PathBuf,
}
impl TrapEnv {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            cwd: "/Workspace".into(),
        })
    }
}
#[async_trait]
impl ExecutionEnv for TrapEnv {}
impl MutatingEnv for TrapEnv {
    fn as_env(&self) -> &dyn ExecutionEnv {
        self
    }
    fn mutation_registry(&self) -> &MutationQueueRegistry {
        panic!("host queue accessed")
    }
}
#[async_trait]
impl Shell for TrapEnv {
    async fn exec<'a>(
        &'a self,
        _: &str,
        _: ShellExecOptions<'a>,
    ) -> Result<ShellOutput, ExecutionError> {
        panic!("host exec")
    }
    async fn cleanup(&self) {
        panic!("host shell cleanup")
    }
}
#[async_trait]
impl FileSystem for TrapEnv {
    fn cwd(&self) -> &Path {
        &self.cwd
    }
    async fn absolute_path(
        &self,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        panic!("host absolute_path")
    }
    async fn join_path(
        &self,
        _: &[&str],
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        panic!("host join_path")
    }
    async fn read_text_file(
        &self,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<String, FileError> {
        panic!("host read_text_file")
    }
    async fn read_text_lines(
        &self,
        _: &str,
        _: Option<usize>,
        _: Option<&CancellationToken>,
    ) -> Result<Vec<String>, FileError> {
        panic!("host read_text_lines")
    }
    async fn read_binary_file(
        &self,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<Vec<u8>, FileError> {
        panic!("host read_binary_file")
    }
    async fn write_file(
        &self,
        _: &str,
        _: FileContent,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        panic!("host write_file")
    }
    async fn append_file(
        &self,
        _: &str,
        _: FileContent,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        panic!("host append_file")
    }
    async fn rename_file(
        &self,
        _: &str,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        panic!("host rename_file")
    }
    async fn file_info(
        &self,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<FileInfo, FileError> {
        panic!("host file_info")
    }
    async fn list_dir(
        &self,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<Vec<FileInfo>, FileError> {
        panic!("host list_dir")
    }
    async fn canonical_path(
        &self,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        panic!("host canonical_path")
    }
    async fn exists(&self, _: &str, _: Option<&CancellationToken>) -> Result<bool, FileError> {
        panic!("host exists")
    }
    async fn create_dir(
        &self,
        _: &str,
        _: bool,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        panic!("host mkdir")
    }
    async fn remove(
        &self,
        _: &str,
        _: bool,
        _: bool,
        _: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        panic!("host remove")
    }
    async fn create_temp_dir(
        &self,
        _: Option<&str>,
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        panic!("host temp dir")
    }
    async fn create_temp_file(
        &self,
        _: &str,
        _: &str,
        _: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        panic!("host temp file")
    }
    async fn cleanup(&self) {
        panic!("host fs cleanup")
    }
}

async fn client_context(backend: Arc<Backend>) -> (Arc<FileIORouter>, ExecutionToolContext) {
    let router = Arc::new(FileIORouter::new());
    router
        .configure(
            FileIOCapabilities {
                read_text: true,
                write_text: true,
            },
            Some(backend),
        )
        .await
        .unwrap();
    let env = TrapEnv::new();
    let ctx = ExecutionToolContext::new(env.clone(), Some(env)).with_file_io(router.clone());
    (router, ctx)
}
async fn run(
    tool: Arc<dyn AgentTool>,
    input: serde_json::Value,
) -> Result<rpi_agent::types::AgentToolResult, rpi_agent::error::AgentError> {
    tool.execute("fixture", input, CancellationToken::new(), Arc::new(|_| {}))
        .await
}
fn output(result: rpi_agent::types::AgentToolResult) -> String {
    match result.content.into_iter().next().expect("text result") {
        rpi_agent::types::TextContentOrImage::Text(text) => text.text,
        rpi_agent::types::TextContentOrImage::Image(_) => panic!("expected text, not image"),
    }
}
fn edit(path: &str, old: &str, new: &str) -> serde_json::Value {
    serde_json::json!({"path": path, "edits": [{"oldText": old, "newText": new}]})
}

#[test]
fn lexical_namespaces_are_explicit_and_case_preserving() {
    for (cwd, path, expected) in [
        ("/Workspace", "@a/../File", "/Workspace/File"),
        ("C:\\Work", "a\\..\\Buffer", "C:/Work/Buffer"),
        ("/Workspace", "D:\\Remote\\..\\Buffer", "D:/Buffer"),
        (
            "/Workspace",
            "\\\\Server\\Share\\a\\..\\Buffer",
            "//Server/Share/Buffer",
        ),
        ("//Server/Share/Work", "../Buffer", "//Server/Share/Buffer"),
        ("C:/Work", "/remote/Buffer", "/remote/Buffer"),
    ] {
        assert_eq!(resolve_client_path(Path::new(cwd), path).unwrap(), expected);
    }
    for path in [
        "C:relative",
        "\\rooted",
        "\\\\?\\C:\\x",
        "//./x",
        "file:///x",
        "~/x",
        "\\\\server",
        "",
    ] {
        assert!(
            resolve_client_path(Path::new("/Workspace"), path).is_err(),
            "{path}"
        );
    }
}

#[tokio::test]
async fn capabilities_four_combinations_keep_disk_and_buffer_separate() {
    for (read_text, write_text) in [(false, false), (true, false), (false, true), (true, true)] {
        let local = Arc::new(InMemoryExecutionEnv::with_cwd("/Workspace".into()));
        let local_path = local.absolute_path("buffer", None).await.unwrap();
        local
            .seed_file(&local_path.to_string_lossy(), b"disk".to_vec())
            .await;
        let backend = Backend::text("buffer");
        let router = Arc::new(FileIORouter::new());
        router
            .configure(
                FileIOCapabilities {
                    read_text,
                    write_text,
                },
                Some(backend.clone()),
            )
            .await
            .unwrap();
        let ctx =
            ExecutionToolContext::new(local.clone(), Some(local.clone())).with_file_io(router);
        let read = output(
            run(
                create_read_tool(&ctx, None),
                serde_json::json!({"path":"buffer"}),
            )
            .await
            .unwrap(),
        );
        assert_eq!(read, if read_text { "buffer" } else { "disk" });
        let write = run(
            create_write_tool(&ctx),
            serde_json::json!({"path":"buffer", "content":"written"}),
        )
        .await;
        assert_eq!(write.is_ok(), !read_text || write_text);
        let edit_result = run(create_edit_tool(&ctx), edit("buffer", "written", "edited")).await;
        assert_eq!(edit_result.is_ok(), read_text == write_text);
        let disk = local
            .read_text_file(&local_path.to_string_lossy(), None)
            .await
            .unwrap();
        let buffer = backend.text.lock().await.clone();
        match (read_text, write_text) {
            (false, false) => {
                assert_eq!(disk, "edited");
                assert_eq!(buffer, "buffer");
            }
            (true, false) => {
                assert_eq!(disk, "disk");
                assert_eq!(buffer, "buffer");
            }
            (false, true) => {
                assert_eq!(disk, "disk");
                assert_eq!(buffer, "written");
            }
            (true, true) => {
                assert_eq!(disk, "disk");
                assert_eq!(buffer, "edited");
            }
        }
    }
}

#[tokio::test]
async fn client_paths_never_probe_host_and_edit_preserves_bom_crlf() {
    let backend = Backend::text("\u{feff}one\r\ntwo\r\n");
    let (_, ctx) = client_context(backend.clone()).await;
    run(
        create_edit_tool(&ctx),
        edit("absent/../unsaved.png", "one\ntwo", "ONE\nTWO"),
    )
    .await
    .unwrap();
    assert_eq!(*backend.text.lock().await, "\u{feff}ONE\r\nTWO\r\n");
    let requests = backend.requests.lock().await.clone();
    assert!(
        matches!(&requests[0], FileIORequest::ReadText { absolute_path, range: FileIOReadRange::Complete, max_bytes: MAX_FILE_IO_BYTES } if absolute_path == "/Workspace/unsaved.png")
    );
    assert!(matches!(&requests[1], FileIORequest::WriteText { .. }));
    // The .png extension is not permission to fetch local binary image bytes.
    let result = run(
        create_read_tool(&ctx, None),
        serde_json::json!({"path":"unsaved.png"}),
    )
    .await
    .unwrap();
    assert!(output(result).contains("ONE"));
    run(
        create_write_tool(&ctx),
        serde_json::json!({"path":"missing/parents/file", "content":"new"}),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn ranged_read_offset_once_sentinel_eof_zero_and_default_cap() {
    let backend = Backend::text("one\ntwo\nthree\nfour\n");
    let (_, ctx) = client_context(backend.clone()).await;
    let read = create_read_tool(&ctx, None);
    let text = output(
        run(
            read.clone(),
            serde_json::json!({"path":"x", "offset":2, "limit":1}),
        )
        .await
        .unwrap(),
    );
    assert!(text.starts_with("two\n\n["));
    assert!(text.contains("offset=3"));
    assert!(text.contains("total line count unknown"));
    assert!(matches!(
        &backend.requests.lock().await[0],
        FileIORequest::ReadText {
            range: FileIOReadRange::Lines {
                offset_1based: 2,
                limit: 2
            },
            ..
        }
    ));
    assert_eq!(
        output(
            run(
                read.clone(),
                serde_json::json!({"path":"x", "offset":4, "limit":1})
            )
            .await
            .unwrap()
        ),
        "four"
    );
    assert_eq!(
        output(
            run(read.clone(), serde_json::json!({"path":"x", "offset":20}))
                .await
                .unwrap()
        ),
        ""
    );
    let zero = output(
        run(read.clone(), serde_json::json!({"path":"x", "limit":0}))
            .await
            .unwrap(),
    );
    assert!(!zero.contains("one"));
    *backend.text.lock().await = (0..2010).map(|_| "x").collect::<Vec<_>>().join("\n");
    let text = output(
        run(read, serde_json::json!({"path":"x", "limit":9000}))
            .await
            .unwrap(),
    );
    assert!(text.contains("offset=2001"));
    assert!(matches!(
        backend.requests.lock().await.last().unwrap(),
        FileIORequest::ReadText {
            range: FileIOReadRange::Lines { limit: 2001, .. },
            ..
        }
    ));
}

#[tokio::test]
async fn blank_line_pages_advance_without_skipping_at_the_byte_boundary() {
    let backend = Backend::text("");
    let (_, ctx) = client_context(backend.clone()).await;
    let read = create_read_tool(&ctx, None);
    for (response, offset, limit, next) in [
        ("\nnext\n".to_owned(), 1, 1, 2),
        ("one\n\nnext\n".to_owned(), 1, 2, 3),
        ("\n\nnext\n".to_owned(), 7, 2, 9),
        (
            format!(
                "{}\n\nnext\n",
                "é".repeat(crate::truncate::DEFAULT_MAX_BYTES / 2)
            ),
            5,
            2,
            6,
        ),
    ] {
        *backend.response.lock().await = Some(FileIOResponse::Text { content: response });
        let text = output(
            run(
                read.clone(),
                serde_json::json!({"path":"x", "offset":offset, "limit":limit}),
            )
            .await
            .unwrap(),
        );
        assert!(
            text.contains(&format!("Use offset={next} to continue")),
            "{text}"
        );
    }
    *backend.response.lock().await = None;
    *backend.text.lock().await = "\n".into();
    assert_eq!(
        output(
            run(read, serde_json::json!({"path":"x", "limit":1}))
                .await
                .unwrap()
        ),
        "\n"
    );
}

#[tokio::test]
async fn read_display_and_raw_utf8_limits_and_write_edit_bounds() {
    let backend = Backend::text(&"x".repeat(crate::truncate::DEFAULT_MAX_BYTES + 1));
    let (_, ctx) = client_context(backend.clone()).await;
    let text = output(
        run(
            create_read_tool(&ctx, None),
            serde_json::json!({"path":"x"}),
        )
        .await
        .unwrap(),
    );
    assert!(text.contains("display limit"));
    *backend.text.lock().await = "é".repeat(MAX_FILE_IO_BYTES / 2 + 1);
    assert!(run(
        create_read_tool(&ctx, None),
        serde_json::json!({"path":"x"})
    )
    .await
    .is_err());
    assert!(run(create_edit_tool(&ctx), edit("x", "é", "a"))
        .await
        .is_err());
    let before = backend.requests.lock().await.len();
    assert!(run(
        create_write_tool(&ctx),
        serde_json::json!({"path":"x", "content":"x".repeat(MAX_FILE_IO_BYTES + 1)})
    )
    .await
    .is_err());
    assert_eq!(backend.requests.lock().await.len(), before);
    *backend.text.lock().await = "a".into();
    assert!(run(
        create_edit_tool(&ctx),
        edit("x", "a", &"x".repeat(MAX_FILE_IO_BYTES + 1))
    )
    .await
    .is_err());
    assert_eq!(*backend.text.lock().await, "a");
}

#[tokio::test]
async fn errors_and_cancellation_never_fall_back_or_retry() {
    let backend = Backend::text("one");
    let (_, ctx) = client_context(backend.clone()).await;
    for code in [
        FileIOErrorCode::Unsupported,
        FileIOErrorCode::Denied,
        FileIOErrorCode::NotFound,
        FileIOErrorCode::InvalidResponse,
        FileIOErrorCode::TooLarge,
        FileIOErrorCode::Timeout,
        FileIOErrorCode::Cancelled,
        FileIOErrorCode::Disconnected,
    ] {
        *backend.error.lock().await = Some(code);
        let before = backend.requests.lock().await.len();
        assert!(run(
            create_read_tool(&ctx, None),
            serde_json::json!({"path":"x"})
        )
        .await
        .is_err());
        assert!(run(
            create_write_tool(&ctx),
            serde_json::json!({"path":"x", "content":"two"})
        )
        .await
        .is_err());
        assert!(run(create_edit_tool(&ctx), edit("x", "one", "two"))
            .await
            .is_err());
        assert_eq!(backend.requests.lock().await.len(), before + 3);
    }
    *backend.error.lock().await = None;
    let signal = CancellationToken::new();
    signal.cancel();
    let before = backend.requests.lock().await.len();
    assert!(create_write_tool(&ctx)
        .execute(
            "cancelled",
            serde_json::json!({"path":"x", "content":"two"}),
            signal,
            Arc::new(|_| {})
        )
        .await
        .is_err());
    assert_eq!(backend.requests.lock().await.len(), before);
    backend.cancel_after.store(true, Ordering::SeqCst);
    assert!(run(
        create_write_tool(&ctx),
        serde_json::json!({"path":"x", "content":"already committed"})
    )
    .await
    .is_err());
    assert_eq!(*backend.text.lock().await, "already committed");
    assert!(run(
        create_edit_tool(&ctx),
        edit("x", "already committed", "three")
    )
    .await
    .is_err());
    assert_eq!(*backend.text.lock().await, "already committed");
}

#[tokio::test]
async fn write_and_edit_share_lexical_queue_and_cancel_does_not_skip_turn() {
    let backend = Backend::text("old");
    let (_, ctx) = client_context(backend.clone()).await;
    backend.hold_write.store(true, Ordering::SeqCst);
    let write = tokio::spawn(run(
        create_write_tool(&ctx),
        serde_json::json!({"path":"dir/../x", "content":"new"}),
    ));
    backend.entered.notified().await;
    let signal = CancellationToken::new();
    let edit_tool = create_edit_tool(&ctx);
    let signal_for_task = signal.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let pending_edit = tokio::spawn(async move {
        started.send(()).unwrap();
        edit_tool
            .execute(
                "edit",
                edit("x", "new", "edited"),
                signal_for_task,
                Arc::new(|_| {}),
            )
            .await
    });
    ready.await.unwrap();
    tokio::task::yield_now().await;
    assert_eq!(backend.requests.lock().await.len(), 1);
    signal.cancel();
    assert!(!pending_edit.is_finished());
    backend.release.notify_one();
    write.await.unwrap().unwrap();
    assert!(pending_edit.await.unwrap().is_err());
    assert_eq!(backend.requests.lock().await.len(), 1);
    run(create_edit_tool(&ctx), edit("x", "new", "edited"))
        .await
        .unwrap();
    assert_eq!(*backend.text.lock().await, "edited");
}

#[tokio::test]
async fn snapshot_freezes_backend_and_invalid_response_is_not_success() {
    let backend = Backend::text("original");
    let (router, _) = client_context(backend.clone()).await;
    let frozen = router.select(FileIOOperation::Edit).await.unwrap().unwrap();
    // Test snapshot stability only: real CLI configuration must require idle.
    router
        .configure(FileIOCapabilities::default(), None)
        .await
        .unwrap();
    assert!(router
        .select(FileIOOperation::Read)
        .await
        .unwrap()
        .is_none());
    frozen
        .write_text("/Workspace/x", "frozen".into(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(*backend.text.lock().await, "frozen");
    *backend.response.lock().await = Some(FileIOResponse::Text {
        content: "not a write ack".into(),
    });
    assert_eq!(
        frozen
            .write_text("/Workspace/x", "bad".into(), &CancellationToken::new())
            .await
            .unwrap_err()
            .code,
        FileIOErrorCode::InvalidResponse
    );
    *backend.response.lock().await = Some(FileIOResponse::Written);
    assert_eq!(
        frozen
            .read_text(
                "/Workspace/x",
                FileIOReadRange::Complete,
                &CancellationToken::new()
            )
            .await
            .unwrap_err()
            .code,
        FileIOErrorCode::InvalidResponse
    );
    *backend.response.lock().await = Some(FileIOResponse::Text {
        content: "one\ntwo\nthree".into(),
    });
    assert_eq!(
        frozen
            .read_text(
                "/Workspace/x",
                FileIOReadRange::Lines {
                    offset_1based: 1,
                    limit: 2
                },
                &CancellationToken::new()
            )
            .await
            .unwrap_err()
            .code,
        FileIOErrorCode::InvalidResponse
    );
}

#[tokio::test]
async fn failed_or_ambiguous_edit_does_not_write() {
    let backend = Backend::text("same\nsame");
    let (_, ctx) = client_context(backend.clone()).await;
    assert!(run(create_edit_tool(&ctx), edit("x", "same", "different"))
        .await
        .is_err());
    assert!(
        run(create_edit_tool(&ctx), edit("x", "missing", "different"))
            .await
            .is_err()
    );
    assert!(backend.requests.lock().await.iter().all(|r| matches!(
        r,
        FileIORequest::ReadText {
            range: FileIOReadRange::Complete,
            ..
        }
    )));
}
