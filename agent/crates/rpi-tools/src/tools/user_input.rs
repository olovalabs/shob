use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolExecutionMode, ToolResultPartial};
use rpi_agent::{AgentError, AgentTool};
use rpi_ai::types::{Schema, Tool};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

pub const QUESTION: &str = "ask_user";
pub const FORM: &str = "request_user_form";

pub struct UserInput(Tool);

impl UserInput {
    pub fn create(name: &str) -> Arc<dyn AgentTool> {
        let options = json!({
            "type": "array", "maxItems": 8,
            "items": {"type": "string", "minLength": 1, "maxLength": 160}
        });
        let label = json!({"type": "string", "minLength": 1, "maxLength": 240});
        let (description, key, properties, required) = if name == QUESTION {
            (
                "Pause and ask the user one or more consequential questions. Offer choices when helpful. Set multiple to allow several choices. The user can also type an answer. Do not ask for credentials.",
                "questions",
                json!({"label": label, "options": options, "multiple": {"type": "boolean"}}),
                json!(["label"]),
            )
        } else {
            (
                "Pause and request structured input. Use this when several named fields are needed. Supports text, number, toggle, select, and multiselect. Supply options for select and multiselect. Do not ask for credentials.",
                "fields",
                json!({
                    "label": label, "options": options,
                    "type": {"type": "string", "enum": ["text", "number", "toggle", "select", "multiselect"]},
                    "required": {"type": "boolean"}
                }),
                json!(["label", "type"]),
            )
        };
        let mut schema = json!({
            "type": "object",
            "properties": {"title": {"type": "string", "minLength": 1, "maxLength": 120}},
            "required": ["title", key],
            "additionalProperties": false
        });
        schema["properties"][key] = json!({
            "type": "array", "minItems": 1, "maxItems": 6,
            "items": {
                "type": "object", "properties": properties, "required": required,
                "additionalProperties": false
            }
        });
        Arc::new(Self(Tool {
            name: name.into(),
            description: description.into(),
            parameters: Schema::new(schema),
            constrained_sampling: None,
        }))
    }
}

#[async_trait]
impl AgentTool for UserInput {
    fn schema(&self) -> &Tool {
        &self.0
    }

    fn label(&self) -> &str {
        "User input"
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Sequential
    }

    async fn execute(
        &self,
        _: &str,
        params: Value,
        _: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let answer = params
            .get("answer")
            .ok_or_else(|| AgentError::Tool("This session has no interactive composer.".into()))?;
        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(answer.to_string())],
            ..Default::default()
        })
    }
}
