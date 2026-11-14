//! Mirrors the tool-definition portion of `packages/agent/src/types.ts`
//! (`AgentTool<TParameters, TDetails>`).
//!
//! TS `AgentTool` extends `Tool` with `label`, optional `prepareArguments`,
//! `execute`, and per-tool `executionMode`. The Rust port keeps the same shape:
//! a struct-less `#[async_trait]` `AgentTool` returning the public `Tool`
//! schema from `rpi_ai`. Tools are held behind `Arc<dyn AgentTool>` so the agent
//! can store + dispatch them without generics.
//!
//! The `execute` signature carries an `on_update` callback (`&dyn Fn`) for
//! streaming partial results. The loop wraps it with an
//! `accepting_updates: Arc<AtomicBool>` gate so calls made after `execute`
//! resolves are no-ops — the late-update suppression invariant (plan §5.3).

use async_trait::async_trait;
use rpi_ai::types::Tool;
use tokio_util::sync::CancellationToken;

use crate::error::AgentError;
use crate::types::{AgentToolResult, ToolExecutionMode, ToolResultPartial};
use std::sync::Arc;

/// A tool the agent can call. Mirrors TS `AgentTool<TParameters, TDetails>`.
///
/// Implementations are `Send + Sync` and conventionally held as
/// `Arc<dyn AgentTool>`. The loop looks up tools by `schema().name` matching a
/// `ToolCall::name`.
#[async_trait]
pub trait AgentTool: Send + Sync {
    /// The provider-facing tool definition (name, description, parameters schema).
    fn schema(&self) -> &Tool;

    /// Human-readable label for UI display.
    fn label(&self) -> &str;

    /// Per-tool execution-mode override. `Sequential` forces one-at-a-time for
    /// the whole batch when any tool in it returns `Sequential`; `Parallel`
    /// (default) allows concurrency.
    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    /// Optional compatibility shim for raw tool-call arguments before schema
    /// validation. Default is identity. Mirrors TS `prepareArguments`.
    fn prepare_arguments(&self, args: serde_json::Value) -> Result<serde_json::Value, AgentError> {
        Ok(args)
    }

    /// Execute the tool call. Throw via `Err(AgentError)` on failure — the loop
    /// encodes the error message into an error `ToolResultMessage`.
    ///
    /// `on_update` streams partial results. It is passed as an `Arc<dyn Fn>`
    /// (not a borrow) so a tool may clone it into a background task that emits
    /// progress after `execute` has returned its main result — e.g. a bash tool
    /// whose throttled output flusher outlives the `await` point. Calls made
    /// after the loop has flipped its `accepting_updates` gate to false are
    /// silently dropped by the loop (late-update suppression, invariant §5.3);
    /// the tool never needs to track settlement itself.
    async fn execute(
        &self,
        tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError>;
}
