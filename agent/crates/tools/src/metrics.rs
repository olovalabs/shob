use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use agent::agent_tool::AgentTool;
use agent::error::AgentError;
use agent::types::{AgentToolResult, TextContentOrImage, ToolExecutionMode, ToolResultPartial};
use ai::types::Tool;
use async_trait::async_trait;
use serde_json::json;
use tokio_util::sync::CancellationToken;

/// Session-local counters; no command arguments, paths, or output are retained.
#[derive(Default)]
pub struct ToolMetrics {
    executions: AtomicU64,
    saved_output_reads: AtomicU64,
    displayed_text_bytes: AtomicU64,
    elapsed_us: AtomicU64,
}

pub fn measure_tool(tool: Arc<dyn AgentTool>, metrics: Arc<ToolMetrics>) -> Arc<dyn AgentTool> {
    Arc::new(MeasuredTool { tool, metrics })
}

struct MeasuredTool {
    tool: Arc<dyn AgentTool>,
    metrics: Arc<ToolMetrics>,
}

impl MeasuredTool {
    fn record(&self, result: &mut AgentToolResult, elapsed_us: u64) {
        let displayed = result
            .content
            .iter()
            .map(|content| match content {
                TextContentOrImage::Text(text) => text.text.len() as u64,
                _ => 0,
            })
            .sum::<u64>();
        let executions = self.metrics.executions.fetch_add(1, Ordering::Relaxed) + 1;
        let rereads = self.metrics.saved_output_reads.fetch_add(
            u64::from(self.tool.schema().name == "output"),
            Ordering::Relaxed,
        ) + u64::from(self.tool.schema().name == "output");
        self.metrics
            .displayed_text_bytes
            .fetch_add(displayed, Ordering::Relaxed);
        self.metrics
            .elapsed_us
            .fetch_add(elapsed_us, Ordering::Relaxed);
        let mut details = std::mem::take(&mut result.details);
        if !details.is_object() {
            details = if details.is_null() {
                json!({})
            } else {
                json!({ "tool_details": details })
            };
        }
        details["performance"] = json!({
            "elapsed_us": elapsed_us,
            "displayed_text_bytes": displayed,
            "session_executions": executions,
            "session_saved_output_reads": rereads,
            "session_displayed_text_bytes": self.metrics.displayed_text_bytes.load(Ordering::Relaxed),
            "session_elapsed_us": self.metrics.elapsed_us.load(Ordering::Relaxed),
        });
        result.details = details;
    }
}

#[async_trait]
impl AgentTool for MeasuredTool {
    fn schema(&self) -> &Tool {
        self.tool.schema()
    }

    fn label(&self) -> &str {
        self.tool.label()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        self.tool.execution_mode()
    }

    fn prepare_arguments(&self, args: serde_json::Value) -> Result<serde_json::Value, AgentError> {
        self.tool.prepare_arguments(args)
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let start = Instant::now();
        let result = self
            .tool
            .execute(tool_call_id, params, signal, on_update)
            .await;
        let elapsed_us = start.elapsed().as_micros().min(u64::MAX as u128) as u64;
        match result {
            Ok(mut result) => {
                self.record(&mut result, elapsed_us);
                Ok(result)
            }
            Err(AgentError::StructuredTool(mut result)) => {
                self.record(&mut result, elapsed_us);
                Err(AgentError::StructuredTool(result))
            }
            Err(error) => {
                self.metrics.executions.fetch_add(1, Ordering::Relaxed);
                self.metrics
                    .elapsed_us
                    .fetch_add(elapsed_us, Ordering::Relaxed);
                Err(error)
            }
        }
    }
}
