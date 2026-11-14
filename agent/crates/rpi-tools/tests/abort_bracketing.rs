//! Mirrors `packages/agent/test/harness/tools.test.ts` "write" abort-bracket case:
//! the mutation queue stays locked until an aborted in-flight write settles — a
//! second writer must NOT proceed while the first is still mid-flight.
//!
//! The TS uses a `BlockingWriteExecutionEnv` subclass of `NodeExecutionEnv` that
//! parks the first write on a deferred. The Rust port can't subclass the concrete
//! `InMemoryExecutionEnv`, so instead we compose: a wrapper `ExecutionEnv` that
//! delegates every method to an inner `InMemoryExecutionEnv` BUT parks the first
//! write (content == "first\n") until a `Notify` fires, and records whether a
//! second write started before the first settled. Same invariant (plan §5.4):
//! the queue guard is held across the closure body; abort does not preempt mutex
//! acquisition — it only makes the closure return `Err(Aborted)` once unblocked.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_tools::{
    ExecutionEnv, ExecutionToolContext, FileContent, FileError, FileInfo, FileSystem,
    InMemoryExecutionEnv, MutatingEnv, MutationQueueRegistry, Shell, ShellExecOptions, ShellOutput,
};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

/// A delegating wrapper that parks the write of "first\n" until released and
/// tracks whether a second write began while the first was still parked.
struct BlockingWriteEnv {
    inner: Arc<InMemoryExecutionEnv>,
    finish_first: Arc<Notify>,
    first_parked: Arc<Notify>,
    second_started: Arc<Mutex<bool>>,
    registry: Arc<MutationQueueRegistry>,
}

impl BlockingWriteEnv {
    fn new(inner: Arc<InMemoryExecutionEnv>) -> Self {
        Self {
            inner,
            finish_first: Arc::new(Notify::new()),
            first_parked: Arc::new(Notify::new()),
            second_started: Arc::new(Mutex::new(false)),
            registry: Arc::new(MutationQueueRegistry::new()),
        }
    }
}

impl MutatingEnv for BlockingWriteEnv {
    fn as_env(&self) -> &dyn ExecutionEnv {
        self
    }
    fn mutation_registry(&self) -> &MutationQueueRegistry {
        // Share the INNER registry so the write tool (which holds this env as
        // Arc<dyn MutatingEnv>) and any direct inner usage serialize on the same
        // per-path mutex. We can't borrow the inner's registry field (private),
        // so this wrapper owns its own — sufficient because all writes go
        // through this wrapper.
        &self.registry
    }
}

#[async_trait]
impl FileSystem for BlockingWriteEnv {
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
    async fn join_path(
        &self,
        parts: &[&str],
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        self.inner.join_path(parts, cancel).await
    }
    async fn read_text_file(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<String, FileError> {
        self.inner.read_text_file(path, cancel).await
    }
    async fn read_text_lines(
        &self,
        path: &str,
        max_lines: Option<usize>,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<String>, FileError> {
        self.inner.read_text_lines(path, max_lines, cancel).await
    }
    async fn read_binary_file(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<u8>, FileError> {
        self.inner.read_binary_file(path, cancel).await
    }
    async fn write_file(
        &self,
        path: &str,
        content: FileContent,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        if matches!(&content, FileContent::Text(s) if s == "first\n") {
            // Park the first write until the test releases it.
            self.first_parked.notify_one();
            self.finish_first.notified().await;
        } else if matches!(&content, FileContent::Text(s) if s == "second\n") {
            *self.second_started.lock().await = true;
        }
        self.inner.write_file(path, content, cancel).await
    }
    async fn append_file(
        &self,
        path: &str,
        content: FileContent,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        self.inner.append_file(path, content, cancel).await
    }
    async fn rename_file(
        &self,
        source: &str,
        dest: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        self.inner.rename_file(source, dest, cancel).await
    }
    async fn file_info(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<FileInfo, FileError> {
        self.inner.file_info(path, cancel).await
    }
    async fn list_dir(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<FileInfo>, FileError> {
        self.inner.list_dir(path, cancel).await
    }
    async fn canonical_path(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        self.inner.canonical_path(path, cancel).await
    }
    async fn exists(
        &self,
        path: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<bool, FileError> {
        self.inner.exists(path, cancel).await
    }
    async fn create_dir(
        &self,
        path: &str,
        recursive: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        self.inner.create_dir(path, recursive, cancel).await
    }
    async fn remove(
        &self,
        path: &str,
        recursive: bool,
        force: bool,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), FileError> {
        self.inner.remove(path, recursive, force, cancel).await
    }
    async fn create_temp_dir(
        &self,
        prefix: Option<&str>,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        self.inner.create_temp_dir(prefix, cancel).await
    }
    async fn create_temp_file(
        &self,
        prefix: &str,
        suffix: &str,
        cancel: Option<&CancellationToken>,
    ) -> Result<PathBuf, FileError> {
        self.inner.create_temp_file(prefix, suffix, cancel).await
    }
    async fn cleanup(&self) {
        // The inner env implements both FileSystem::cleanup and Shell::cleanup;
        // call FileSystem's (both are best-effort no-ops on InMemory).
        let inner: &dyn FileSystem = &*self.inner;
        inner.cleanup().await;
    }
}

#[async_trait]
impl Shell for BlockingWriteEnv {
    async fn exec<'a>(
        &'a self,
        command: &str,
        options: ShellExecOptions<'a>,
    ) -> Result<ShellOutput, rpi_tools::ExecutionError> {
        self.inner.exec(command, options).await
    }
    async fn cleanup(&self) {
        let inner: &dyn Shell = &*self.inner;
        inner.cleanup().await;
    }
}

impl ExecutionEnv for BlockingWriteEnv {}

async fn run_write(
    tool: Arc<dyn AgentTool>,
    params: serde_json::Value,
    signal: Option<CancellationToken>,
) -> Result<rpi_agent::types::AgentToolResult, rpi_agent::error::AgentError> {
    let signal = signal.unwrap_or_default();
    let on_update = Arc::new(|_p| ());
    let prepared = tool
        .prepare_arguments(params.clone())
        .unwrap_or_else(|_| params);
    tool.execute("write-x", prepared, signal, on_update).await
}

#[tokio::test]
async fn queue_stays_locked_until_aborted_write_settles() {
    let inner = Arc::new(InMemoryExecutionEnv::with_cwd("/tmp/work".into()));
    let wrapper = Arc::new(BlockingWriteEnv::new(inner.clone()));
    let env_dyn: Arc<dyn ExecutionEnv> = wrapper.clone() as Arc<dyn ExecutionEnv>;
    let mut_env: Arc<dyn MutatingEnv> = wrapper.clone() as Arc<dyn MutatingEnv>;
    let ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));

    let tool = rpi_tools::create_write_tool(&ctx);

    // First write parks inside the queue closure. Its cancellation token lets
    // the test abort it — but abort must NOT unblock the queue: the parked
    // write only returns Err(Aborted) once `finish_first` fires.
    let first_token = CancellationToken::new();
    let first_handle = {
        let tool = tool.clone();
        let tok = first_token.clone();
        tokio::spawn(async move {
            run_write(
                tool,
                serde_json::json!({ "path": "file.txt", "content": "first\n" }),
                Some(tok),
            )
            .await
        })
    };

    // Wait until the first write is parked inside the closure.
    wrapper.first_parked.notified().await;
    // Now abort the first write — it must NOT release the queue yet.
    first_token.cancel();

    // Kick off the second write; it should block on the per-path mutex.
    let second_started = wrapper.second_started.clone();
    let second_handle = {
        let tool = tool.clone();
        tokio::spawn(async move {
            run_write(
                tool,
                serde_json::json!({ "path": "file.txt", "content": "second\n" }),
                None,
            )
            .await
        })
    };

    // Give the second write a moment to try — it must not have started writing.
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    {
        let started = *second_started.lock().await;
        assert!(
            !started,
            "second write must not start while first is parked (abort does not unblock the queue)"
        );
    }

    // Release the first write; it observes the abort and returns Err, releasing
    // the queue. The second write then proceeds.
    wrapper.finish_first.notify_one();
    first_handle
        .await
        .expect("first task join")
        .expect_err("first write was aborted → Err");

    second_handle
        .await
        .expect("second task join")
        .expect("second write ok");

    // The file reflects the second (winning) write only.
    let abs = inner.absolute_path("file.txt", None).await.expect("abs");
    let text = inner
        .read_text_file(&abs.to_string_lossy(), None)
        .await
        .expect("read");
    assert_eq!(text, "second\n");
}
