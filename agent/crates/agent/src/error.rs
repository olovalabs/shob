//! Mirrors `packages/agent/src` error surface. Pi's agent layer reports failures
//! via thrown errors that the loop catches and encodes into tool results / event
//! sequences; Rust encodes the same cases as a per-crate `thiserror` enum.

/// Failures raised inside the agent loop. Cannot cross the provider boundary
/// (provider failures arrive as `AssistantMessageEvent::Error`); these surface
/// from tool execution, argument validation, queue/state misuse, or abort.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AgentError {
    #[error("tool error: {0}")]
    Tool(String),

    /// A failed tool that still owns native content and accounting metadata.
    #[error("structured tool error")]
    StructuredTool(Box<crate::types::AgentToolResult>),

    #[error("validation error: {0}")]
    Validation(String),

    #[error("operation aborted")]
    Abort,

    #[error("provider error: {0}")]
    Provider(String),

    #[error("queue error: {0}")]
    Queue(String),

    #[error("invalid state: {0}")]
    State(String),
}

impl AgentError {
    pub fn tool(message: impl Into<String>) -> Self {
        AgentError::Tool(message.into())
    }

    /// True when the error originated from an abort. Used by the loop to decide
    /// the terminal event shape (`Aborted` vs `Error`).
    pub fn is_abort(&self) -> bool {
        matches!(self, AgentError::Abort)
    }
}
