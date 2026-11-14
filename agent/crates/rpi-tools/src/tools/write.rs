//! Mirrors `packages/agent/src/harness/tools/write.ts` — the built-in `write`
//! `AgentTool`. Writes content to a file (creating parent dirs), serialized per
//! canonical path via the file-mutation-queue, with pre/post abort bracketing.
//!
//! Invariant (plan §5.4): abort does NOT unblock the queue — the caller still
//! waits for its turn, then the closure observes the token and returns
//! `Err(Aborted)`. The pre/post abort checks around `write_file` mirror the TS
//! `if (signal?.aborted) throw` lines.

use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::FutureExt;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use rpi_ai::types::{ConstrainedSamplingConfig, ConstrainedStrictness, Tool};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::env::FileContent;
use crate::error::{FileError, FileErrorCode};
use crate::file_io::{resolve_client_path, FileIOOperation, FileIORouter};
use crate::file_mutation_queue::{with_file_mutation_queue, MutatingEnv};
use crate::path_utils::resolve_tool_path;
use crate::tools::tool_context::ExecutionToolContext;

/// Input for the write tool. Mirrors TS `WriteToolInput`.
#[derive(Debug, Clone, JsonSchema, Deserialize)]
pub struct WriteInput {
    /// Path to the file to write (relative or absolute).
    pub path: String,
    /// Content to write to the file.
    pub content: String,
}

/// The built-in `write` tool. Holds the mutating env (for the queue + writes).
pub struct WriteTool {
    schema: Tool,
    env: Arc<dyn MutatingEnv>,
    file_io: Option<Arc<FileIORouter>>,
}

impl WriteTool {
    fn schema() -> Tool {
        let params = schemars::schema_for!(WriteInput);
        Tool {
            name: "write".to_string(),
            description: "Write content to a file. Creates the file if it doesn't exist, \
                          overwrites if it does. Automatically creates parent directories."
                .to_string(),
            parameters: rpi_ai::types::Schema::new(
                serde_json::to_value(params).unwrap_or_default(),
            ),
            constrained_sampling: Some(ConstrainedSamplingConfig::JsonSchema {
                strict: ConstrainedStrictness::Prefer,
            }),
        }
    }
}

/// Build the `write` tool from a context. Requires a mutation-capable env.
/// Mirrors TS `createWriteTool`.
pub fn create_write_tool(context: &ExecutionToolContext) -> Arc<dyn AgentTool> {
    let env = context
        .mutating_env()
        .expect("write tool requires a mutation-capable ExecutionToolContext")
        .clone();
    Arc::new(WriteTool {
        schema: WriteTool::schema(),
        env,
        file_io: context.file_io().cloned(),
    })
}

#[async_trait]
impl AgentTool for WriteTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "write"
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: WriteInput = serde_json::from_value(params)
            .map_err(|e| AgentError::Validation(format!("write input invalid: {e}")))?;

        // Freeze authority before any local path resolution or queue access.
        if let Some(router) = &self.file_io {
            if let Some(client) = router
                .select(FileIOOperation::Write)
                .await
                .map_err(|e| file_err_to_agent(e.into()))?
            {
                let abs = resolve_client_path(self.env.as_env().cwd(), &input.path)
                    .map_err(|e| file_err_to_agent(e.into()))?;
                let path_for_msg = input.path;
                let abs_for_closure = abs.clone();
                router
                    .with_mutation_queue(&abs, &signal, move |cancel| {
                        async move {
                            client
                                .write_text(&abs_for_closure, input.content, cancel)
                                .await
                                .map_err(FileError::from)
                        }
                        .boxed()
                    })
                    .await
                    .map_err(file_err_to_agent)?;
                return Ok(AgentToolResult {
                    content: vec![TextContentOrImage::text(format!(
                        "Successfully wrote to {path_for_msg}"
                    ))],
                    details: serde_json::Value::Null,
                    ..Default::default()
                });
            }
        }

        let abs = resolve_tool_path(self.env.as_env(), &input.path, Some(&signal))
            .await
            .map_err(file_err_to_agent)?;
        let abs_str = abs.clone();
        let path_for_msg = input.path.clone();

        // The mutation-queue closure. Abort does NOT unblock the queue: we still
        // acquire the lock, then check the token before+after the write.
        let content = input.content;
        let env = self.env.clone();
        // `with_file_mutation_queue` borrows the env for the whole call, so keep
        // a separate owned clone for the borrow reference and let the closure
        // capture/move the other.
        let env_ref = env.clone();
        let abs_for_closure = abs_str.clone();
        let result: Result<(), FileError> = with_file_mutation_queue(
            &*env_ref,
            &abs_str,
            &signal,
            move |cancel: &CancellationToken| -> BoxFuture<'_, Result<(), FileError>> {
                let env = env.clone();
                let content = content.clone();
                let abs_str = abs_for_closure.clone();
                async move {
                    check_abort(cancel)?;
                    env.as_env()
                        .write_file(&abs_str, FileContent::Text(content), Some(cancel))
                        .await?;
                    check_abort(cancel)?;
                    Ok(())
                }
                .boxed()
            },
        )
        .await;

        result.map_err(file_err_to_agent)?;

        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(format!(
                "Successfully wrote to {path_for_msg}"
            ))],
            details: serde_json::Value::Null,
            ..Default::default()
        })
    }
}

/// Check the cancel token, returning `Err(FileError::aborted())` when set.
/// Mirrors the TS `if (signal?.aborted) throw new Error("Operation aborted")`.
fn check_abort(cancel: &CancellationToken) -> Result<(), FileError> {
    if cancel.is_cancelled() {
        return Err(FileError::new(FileErrorCode::Aborted, "Operation aborted"));
    }
    Ok(())
}

fn file_err_to_agent(e: FileError) -> AgentError {
    AgentError::Tool(e.to_string())
}
