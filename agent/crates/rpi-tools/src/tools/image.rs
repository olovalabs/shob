//! Image processing tool for the agent.
//!
//! Provides image processing capabilities: resize, convert, and get info.
//! Mirrors the image processing utilities from upstream.

use std::sync::Arc;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine};
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use rpi_ai::types::{ConstrainedSamplingConfig, ConstrainedStrictness, Schema, Tool};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::image_processing::{
    detect_image_mime, get_image_dimensions, process_image, ImageProcessingOptions, OutputFormat,
};

/// Input for the image tool.
#[derive(Debug, Clone, JsonSchema, Deserialize)]
pub struct ImageToolInput {
    /// The action to perform: "info", "resize", "convert", or "process".
    pub action: String,
    /// Base64-encoded image data (required for all actions).
    pub image_data: String,
    /// Target width in pixels (for resize/process).
    #[serde(default)]
    pub width: Option<u32>,
    /// Target height in pixels (for resize/process).
    #[serde(default)]
    pub height: Option<u32>,
    /// Output format: "png", "jpeg", or "webp" (for convert/process).
    #[serde(default)]
    pub format: Option<String>,
    /// JPEG/WebP quality 1-100 (for convert/process).
    #[serde(default)]
    pub quality: Option<u8>,
    /// Whether to strip EXIF metadata (default: true).
    #[serde(default)]
    pub strip_exif: Option<bool>,
}

/// Image tool implementation.
pub struct ImageTool {
    schema: Tool,
    max_dimension: u32,
    default_format: String,
}

impl ImageTool {
    /// Create a new image tool.
    pub fn new(max_dimension: u32, default_format: &str) -> Self {
        Self {
            schema: Self::build_schema(),
            max_dimension,
            default_format: default_format.to_string(),
        }
    }

    fn build_schema() -> Tool {
        let params = schemars::schema_for!(ImageToolInput);
        Tool {
            name: "image".to_string(),
            description: "Process, resize, convert, and get information about images. \
                         Supports PNG, JPEG, WebP, GIF, and BMP formats. \
                         Actions: 'info' (get metadata), 'resize' (scale dimensions), \
                         'convert' (change format), 'process' (resize + convert + strip EXIF)."
                .to_string(),
            parameters: Schema::new(serde_json::to_value(params).unwrap_or_default()),
            constrained_sampling: Some(ConstrainedSamplingConfig::JsonSchema {
                strict: ConstrainedStrictness::Prefer,
            }),
        }
    }

    fn decode_base64(data: &str) -> Result<Vec<u8>, AgentError> {
        STANDARD
            .decode(data)
            .map_err(|e| AgentError::tool(format!("Invalid base64: {e}")))
    }

    fn encode_base64(data: &[u8]) -> String {
        STANDARD.encode(data)
    }

    fn parse_format(fmt: &str, quality: Option<u8>) -> Result<OutputFormat, AgentError> {
        match fmt {
            "png" => Ok(OutputFormat::Png),
            "jpeg" | "jpg" => Ok(OutputFormat::Jpeg {
                quality: quality.unwrap_or(85),
            }),
            "webp" => Ok(OutputFormat::WebP {
                quality: quality.unwrap_or(85),
            }),
            other => Err(AgentError::tool(format!(
                "Unsupported format: {other}. Use png, jpeg, or webp."
            ))),
        }
    }

    fn handle_info(&self, input: &ImageToolInput) -> Result<AgentToolResult, AgentError> {
        let bytes = Self::decode_base64(&input.image_data)?;
        let mime = detect_image_mime(&bytes).unwrap_or("unknown");
        let (width, height) = get_image_dimensions(&bytes)
            .map_err(|e| AgentError::tool(format!("Failed to read image: {e}")))?;

        let output = serde_json::json!({
            "success": true,
            "width": width,
            "height": height,
            "mime_type": mime,
            "size_bytes": bytes.len(),
        });

        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(format!(
                "Image info: {}x{} {} ({} bytes)",
                width,
                height,
                mime,
                bytes.len()
            ))],
            details: output,
            usage: None,
            added_tool_names: vec![],
            terminate: false,
        })
    }

    fn handle_resize(&self, input: &ImageToolInput) -> Result<AgentToolResult, AgentError> {
        let bytes = Self::decode_base64(&input.image_data)?;

        let options = ImageProcessingOptions {
            max_width: input.width.map(|w| w.min(self.max_dimension)),
            max_height: input.height.map(|h| h.min(self.max_dimension)),
            format: None,
            strip_exif: input.strip_exif.unwrap_or(true),
        };

        let processed = process_image(&bytes, options)
            .map_err(|e| AgentError::tool(format!("Resize failed: {e}")))?;

        let encoded = Self::encode_base64(&processed);
        let (width, height) = get_image_dimensions(&processed).unwrap_or((0, 0));

        let output = serde_json::json!({
            "success": true,
            "image_data": encoded,
            "width": width,
            "height": height,
            "size_bytes": processed.len(),
        });

        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(format!(
                "Image resized to {}x{} ({} bytes)",
                width,
                height,
                processed.len()
            ))],
            details: output,
            usage: None,
            added_tool_names: vec![],
            terminate: false,
        })
    }

    fn handle_convert(&self, input: &ImageToolInput) -> Result<AgentToolResult, AgentError> {
        let bytes = Self::decode_base64(&input.image_data)?;
        let format_str = input.format.as_deref().unwrap_or(&self.default_format);
        let format = Self::parse_format(format_str, input.quality)?;

        let options = ImageProcessingOptions {
            max_width: None,
            max_height: None,
            format: Some(format),
            strip_exif: input.strip_exif.unwrap_or(true),
        };

        let processed = process_image(&bytes, options)
            .map_err(|e| AgentError::tool(format!("Convert failed: {e}")))?;

        let encoded = Self::encode_base64(&processed);
        let mime = detect_image_mime(&processed).unwrap_or("unknown");

        let output = serde_json::json!({
            "success": true,
            "image_data": encoded,
            "mime_type": mime,
            "size_bytes": processed.len(),
        });

        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(format!(
                "Image converted to {} ({} bytes)",
                mime,
                processed.len()
            ))],
            details: output,
            usage: None,
            added_tool_names: vec![],
            terminate: false,
        })
    }

    fn handle_process(&self, input: &ImageToolInput) -> Result<AgentToolResult, AgentError> {
        let bytes = Self::decode_base64(&input.image_data)?;

        let format = if let Some(fmt) = &input.format {
            Some(Self::parse_format(fmt, input.quality)?)
        } else {
            None
        };

        let options = ImageProcessingOptions {
            max_width: input.width.map(|w| w.min(self.max_dimension)),
            max_height: input.height.map(|h| h.min(self.max_dimension)),
            format,
            strip_exif: input.strip_exif.unwrap_or(true),
        };

        let processed = process_image(&bytes, options)
            .map_err(|e| AgentError::tool(format!("Process failed: {e}")))?;

        let encoded = Self::encode_base64(&processed);
        let (width, height) = get_image_dimensions(&processed).unwrap_or((0, 0));
        let mime = detect_image_mime(&processed).unwrap_or("unknown");

        let output = serde_json::json!({
            "success": true,
            "image_data": encoded,
            "width": width,
            "height": height,
            "mime_type": mime,
            "size_bytes": processed.len(),
        });

        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(format!(
                "Image processed: {}x{} {} ({} bytes)",
                width,
                height,
                mime,
                processed.len()
            ))],
            details: output,
            usage: None,
            added_tool_names: vec![],
            terminate: false,
        })
    }
}

#[async_trait]
impl AgentTool for ImageTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }

    fn label(&self) -> &str {
        "image"
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: ImageToolInput = serde_json::from_value(params)
            .map_err(|e| AgentError::tool(format!("image input invalid: {e}")))?;

        match input.action.as_str() {
            "info" => self.handle_info(&input),
            "resize" => self.handle_resize(&input),
            "convert" => self.handle_convert(&input),
            "process" => self.handle_process(&input),
            other => Err(AgentError::tool(format!(
                "Unknown action: {other}. Use info, resize, convert, or process."
            ))),
        }
    }
}

/// Create an image tool with default settings.
pub fn create_image_tool() -> Arc<dyn AgentTool> {
    Arc::new(ImageTool::new(2048, "png"))
}

/// Create an image tool with custom settings.
pub fn create_image_tool_with_config(
    max_dimension: u32,
    default_format: &str,
) -> Arc<dyn AgentTool> {
    Arc::new(ImageTool::new(max_dimension, default_format))
}

/// Configuration type for the image tool (max_dimension, default_format).
pub type ImageToolConfig = (u32, String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_format() {
        assert!(matches!(
            ImageTool::parse_format("png", None),
            Ok(OutputFormat::Png)
        ));
        assert!(matches!(
            ImageTool::parse_format("jpeg", Some(90)),
            Ok(OutputFormat::Jpeg { quality: 90 })
        ));
        assert!(matches!(
            ImageTool::parse_format("webp", Some(80)),
            Ok(OutputFormat::WebP { quality: 80 })
        ));
        assert!(ImageTool::parse_format("bmp", None).is_err());
    }

    #[test]
    fn test_decode_base64() {
        // Valid base64
        assert!(ImageTool::decode_base64("aGVsbG8=").is_ok());
        // Invalid base64
        assert!(ImageTool::decode_base64("not-valid-base64!!!").is_err());
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let original = b"test image data";
        let encoded = ImageTool::encode_base64(original);
        let decoded = ImageTool::decode_base64(&encoded).unwrap();
        assert_eq!(original.to_vec(), decoded);
    }
}
