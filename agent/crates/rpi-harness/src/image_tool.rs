//! Normalize tool images before they reach the model or session transcript.

use async_trait::async_trait;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolExecutionMode, ToolResultPartial};
use rpi_agent::{AgentError, AgentTool};
use rpi_ai::types::{Content, Tool};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(crate) fn wrap(tool: Arc<dyn AgentTool>) -> Arc<dyn AgentTool> {
    Arc::new(ImageTool(tool))
}

struct ImageTool(Arc<dyn AgentTool>);

#[async_trait]
impl AgentTool for ImageTool {
    fn schema(&self) -> &Tool {
        self.0.schema()
    }
    fn label(&self) -> &str {
        self.0.label()
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        self.0.execution_mode()
    }
    fn prepare_arguments(&self, args: serde_json::Value) -> Result<serde_json::Value, AgentError> {
        self.0.prepare_arguments(args)
    }
    async fn execute(
        &self,
        id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let (mut result, is_error) =
            match self.0.execute(id, params, signal.clone(), on_update).await {
                Ok(result) => (result, false),
                Err(AgentError::StructuredTool(result)) => (*result, true),
                Err(error) => return Err(error),
            };
        let outcome = |result| {
            if is_error {
                Err(AgentError::StructuredTool(Box::new(result)))
            } else {
                Ok(result)
            }
        };
        if !result
            .content
            .iter()
            .any(|block| matches!(block, TextContentOrImage::Image(_)))
        {
            return outcome(result);
        }
        result = tokio::task::spawn_blocking(move || {
            let mut blocks: Vec<Content> = std::mem::take(&mut result.content)
                .into_iter()
                .map(|block| match block {
                    TextContentOrImage::Text(text) => Content::Text(text),
                    TextContentOrImage::Image(image) => Content::Image(image),
                })
                .collect();
            rpi_tools::image_processing::normalize_image_blocks(&mut blocks);
            result.content = blocks
                .into_iter()
                .filter_map(|block| match block {
                    Content::Text(text) => Some(TextContentOrImage::Text(text)),
                    Content::Image(image) => Some(TextContentOrImage::Image(image)),
                    _ => None,
                })
                .collect();
            result
        })
        .await
        .map_err(|error| AgentError::Tool(format!("Image processing failed: {error}")))?;
        if signal.is_cancelled() {
            return Err(AgentError::Tool("Image processing cancelled".into()));
        }
        outcome(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct InvalidImageTool;
    #[async_trait]
    impl AgentTool for InvalidImageTool {
        fn schema(&self) -> &Tool {
            panic!("test does not request schema")
        }
        fn label(&self) -> &str {
            "image"
        }
        async fn execute(
            &self,
            _: &str,
            _: serde_json::Value,
            _: CancellationToken,
            _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
        ) -> Result<AgentToolResult, AgentError> {
            Ok(AgentToolResult {
                content: vec![TextContentOrImage::Image(rpi_ai::types::ImageContent {
                    kind: rpi_ai::types::ImageContentType,
                    data: "invalid".into(),
                    mime_type: "image/bmp".into(),
                })],
                details: serde_json::json!({"retained": true}),
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn tool_image_failure_is_visible_and_does_not_discard_details() {
        let tool = wrap(Arc::new(InvalidImageTool));
        let result = tool
            .execute(
                "id",
                serde_json::json!({}),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap();
        assert_eq!(result.details, serde_json::json!({"retained": true}));
        assert!(
            matches!(&result.content[0], TextContentOrImage::Text(text) if text.text.contains("Image omitted"))
        );
    }

    struct ResultTool(Result<AgentToolResult, AgentError>);
    #[async_trait]
    impl AgentTool for ResultTool {
        fn schema(&self) -> &Tool {
            panic!("test does not request schema")
        }
        fn label(&self) -> &str {
            "result"
        }
        async fn execute(
            &self,
            _: &str,
            _: serde_json::Value,
            _: CancellationToken,
            _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
        ) -> Result<AgentToolResult, AgentError> {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn structured_error_images_use_native_normalization_and_retain_metadata() {
        let mut usage = rpi_ai::types::Usage::zero();
        usage.input = 3;
        usage.total_tokens = 3;
        let original = AgentToolResult {
            content: vec![
                TextContentOrImage::text("native failure"),
                TextContentOrImage::Image(rpi_ai::types::ImageContent {
                    kind: rpi_ai::types::ImageContentType,
                    data: "invalid".into(),
                    mime_type: "image/bmp".into(),
                }),
            ],
            details: serde_json::json!({"retained": true, "nested": {"id": 7}}),
            usage: Some(usage),
            added_tool_names: vec!["z".into(), "a".into()],
            terminate: true,
        };
        let execute = |result| async move {
            wrap(Arc::new(ResultTool(result)))
                .execute(
                    "id",
                    serde_json::json!({}),
                    CancellationToken::new(),
                    Arc::new(|_| {}),
                )
                .await
        };
        let success = execute(Ok(original.clone())).await.unwrap();
        let Err(AgentError::StructuredTool(result)) =
            execute(Err(AgentError::StructuredTool(Box::new(original.clone())))).await
        else {
            panic!("normalization must retain the structured error flag")
        };
        assert_eq!(result.content, success.content);
        assert_eq!(result.content[0], original.content[0]);
        assert!(
            matches!(&result.content[1], TextContentOrImage::Text(text) if text.text.contains("Image omitted"))
        );
        assert_eq!(result.details, original.details);
        assert_eq!(
            serde_json::to_value(&result.usage).unwrap(),
            serde_json::to_value(&original.usage).unwrap()
        );
        assert_eq!(result.added_tool_names, original.added_tool_names);
        assert!(result.terminate);
    }

    #[tokio::test]
    async fn non_image_structured_errors_and_ordinary_errors_are_unchanged() {
        let original = AgentToolResult::text("structured failure without image");
        let result = wrap(Arc::new(ResultTool(Err(AgentError::StructuredTool(
            Box::new(original.clone()),
        )))))
        .execute(
            "id",
            serde_json::json!({}),
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await;
        assert!(
            matches!(result, Err(AgentError::StructuredTool(result)) if result.content == original.content)
        );
        let result = wrap(Arc::new(ResultTool(Err(AgentError::tool(
            "ordinary failure",
        )))))
        .execute(
            "id",
            serde_json::json!({}),
            CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await;
        assert!(matches!(result, Err(AgentError::Tool(message)) if message == "ordinary failure"));
    }
}
