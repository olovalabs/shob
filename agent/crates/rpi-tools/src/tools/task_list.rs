use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolExecutionMode, ToolResultPartial};
use rpi_agent::{AgentError, AgentTool};
use rpi_ai::types::{Schema, Tool};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

pub const NAME: &str = "update_task_list";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct TaskList {
    pub tasks: Vec<Task>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct Task {
    pub text: String,
    pub status: TaskStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

pub struct TaskListTool(Tool);

impl TaskListTool {
    pub fn create() -> Arc<dyn AgentTool> {
        Arc::new(Self(Tool {
            name: NAME.into(),
            description: "Show and update the user's live checklist. Use this before starting substantial multi-step work. Supply the entire ordered list each time, keeping finished steps visible. Start with one step in_progress. Work on that step, then mark it completed and start the next one as soon as the current step is verified. Leave unfinished steps unchecked. Omit the checklist for simple requests unless asked.".into(),
            parameters: Schema::new(json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 12,
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": {"type": "string", "minLength": 1, "maxLength": 180},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                            },
                            "required": ["text", "status"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["tasks"],
                "additionalProperties": false
            })),
            constrained_sampling: None,
        }))
    }
}

#[async_trait]
impl AgentTool for TaskListTool {
    fn schema(&self) -> &Tool {
        &self.0
    }

    fn label(&self) -> &str {
        "Update task list"
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let mut list: TaskList = serde_json::from_value(params)
            .map_err(|error| AgentError::Validation(error.to_string()))?;
        let mut active = false;
        for task in &mut list.tasks {
            task.text = task.text.split_whitespace().collect::<Vec<_>>().join(" ");
            if task.text.is_empty() {
                return Err(AgentError::Validation(
                    "Task text must not be blank.".into(),
                ));
            }
            if task.status == TaskStatus::InProgress {
                if active {
                    return Err(AgentError::Validation(
                        "Only one task can be in progress.".into(),
                    ));
                }
                active = true;
            }
        }
        // The complete snapshot is retained in the conversation's tool result.
        let snapshot =
            serde_json::to_string(&list).map_err(|error| AgentError::Tool(error.to_string()))?;
        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(snapshot)],
            ..Default::default()
        })
    }
}
