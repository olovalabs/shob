//! Mirrors `packages/agent/src/harness/tools/read.ts` — the built-in `read`
//! `AgentTool`. Reads a file (text or image), applies head-truncation with a
//! continuation hint, and detects supported image MIME types. An optional
//! `ReadImageProcessor` seam supports resize/convert implementations; without
//! one, BMP images yield an omitted notice and other formats are inlined.

use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use rpi_ai::types::{
    ConstrainedSamplingConfig, ConstrainedStrictness, ImageContent, ImageContentType, Tool,
};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::env::ExecutionEnv;
use crate::file_io::{
    resolve_client_path, FileIOOperation, FileIOReadRange, FileIORouter, FileIOSnapshot,
};
use crate::image::{detect_supported_image_mime_type, encode_base64};
use crate::path_utils::resolve_read_tool_path;
use crate::tools::tool_context::ExecutionToolContext;
use crate::truncate::{
    format_size, truncate_head, TruncationOptions, TruncationResult, DEFAULT_MAX_BYTES,
    DEFAULT_MAX_LINES,
};
/// Input for the read tool. Mirrors TS `ReadToolInput`. `offset` is 1-indexed.
#[derive(Debug, Clone, JsonSchema, Deserialize)]
pub struct ReadInput {
    /// Path to the file to read (relative or absolute).
    pub path: String,
    /// Line number to start reading from (1-indexed).
    #[serde(default)]
    pub offset: Option<u32>,
    /// Maximum number of lines to read.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Structured details for the read tool. Mirrors TS `ReadToolDetails`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReadToolDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
}

/// Options for `create_read_tool`. Mirrors TS `ReadToolOptions`.
pub struct ReadToolOptions {
    /// Whether an injected image processor should resize images. Default true.
    pub auto_resize_images: bool,
    /// Optional image conversion/resizing implementation.
    pub image_processor: Option<Arc<dyn ReadImageProcessor>>,
}

impl Clone for ReadToolOptions {
    fn clone(&self) -> Self {
        Self {
            auto_resize_images: self.auto_resize_images,
            image_processor: self.image_processor.clone(),
        }
    }
}

impl std::fmt::Debug for ReadToolOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadToolOptions")
            .field("auto_resize_images", &self.auto_resize_images)
            .field("image_processor", &self.image_processor.is_some())
            .finish()
    }
}

impl Default for ReadToolOptions {
    fn default() -> Self {
        Self {
            auto_resize_images: true,
            image_processor: None,
        }
    }
}

/// Optional image-conversion seam. Mirrors TS `ReadImageProcessor`.
pub trait ReadImageProcessor: Send + Sync {
    fn process(
        &self,
        bytes: &[u8],
        mime_type: &str,
        auto_resize: bool,
    ) -> Result<ProcessedImage, String>;
}

/// Result of a successful [`ReadImageProcessor::process`].
pub struct ProcessedImage {
    pub data: String,
    pub mime_type: String,
    pub hints: Vec<String>,
}

/// The built-in `read` tool. Holds an `Arc<dyn ExecutionEnv>` (read-only view)
/// and an optional image processor.
pub struct ReadTool {
    schema: Tool,
    env: Arc<dyn ExecutionEnv>,
    file_io: Option<Arc<FileIORouter>>,
    options: ReadToolOptions,
    image_processor: Option<Arc<dyn ReadImageProcessor>>,
}

impl ReadTool {
    fn schema() -> Tool {
        let params = schemars::schema_for!(ReadInput);
        Tool {
            name: "read".to_string(),
            description: format!(
                "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). \
                 Images are sent as attachments. For text files, output is truncated to {DEFAULT_MAX_LINES} \
                 lines or {kb}KB (whichever is hit first). Use offset/limit for large files. When you need \
                 the full file, continue with offset until complete.",
                kb = DEFAULT_MAX_BYTES / 1024
            ),
            parameters: rpi_ai::types::Schema::new(serde_json::to_value(params).unwrap_or_default()),
            constrained_sampling: Some(ConstrainedSamplingConfig::JsonSchema { strict: ConstrainedStrictness::Prefer }),
        }
    }
}

/// Build the `read` tool from a context. Mirrors TS `createReadTool`.
pub fn create_read_tool(
    context: &ExecutionToolContext,
    options: Option<ReadToolOptions>,
) -> Arc<dyn AgentTool> {
    let options = options.unwrap_or_default();
    let image_processor = options.image_processor.clone();
    Arc::new(ReadTool {
        schema: ReadTool::schema(),
        env: context.env().clone(),
        file_io: context.file_io().cloned(),
        options,
        image_processor,
    })
}

#[async_trait]
impl AgentTool for ReadTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "read"
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: ReadInput = serde_json::from_value(params)
            .map_err(|e| AgentError::Validation(format!("read input invalid: {e}")))?;

        if let Some(router) = &self.file_io {
            if let Some(client) = router
                .select(FileIOOperation::Read)
                .await
                .map_err(|e| file_err_to_agent(e.into()))?
            {
                let abs = resolve_client_path(self.env.cwd(), &input.path)
                    .map_err(|e| file_err_to_agent(e.into()))?;
                // Text only: never sniff extensions or fetch host image bytes.
                return client_text_result(&client, &abs, &input, &signal).await;
            }
        }

        let abs = resolve_read_tool_path(&*self.env, &input.path, Some(&signal))
            .await
            .map_err(file_err_to_agent)?;
        let bytes = self
            .env
            .read_binary_file(&abs, Some(&signal))
            .await
            .map_err(file_err_to_agent)?;

        // Image path.
        if let Some(mime) = detect_supported_image_mime_type(&bytes) {
            return Ok(image_result(
                mime,
                &bytes,
                &self.options,
                self.image_processor.as_deref(),
            ));
        }

        // Text path.
        let text_content = String::from_utf8_lossy(&bytes).into_owned();
        let all_lines: Vec<&str> = text_content.split('\n').collect();
        // Mirror TS: `textContent.split("\n")` keeps a trailing "" when the
        // content ends with '\n'; the line-count + slice math below uses the raw
        // split (NOT `split_lines_for_counting`), so we keep it as-is.
        let total_file_lines = all_lines.len();
        let start_line = input
            .offset
            .map(|o| (o.saturating_sub(1)) as usize)
            .unwrap_or(0);
        let start_line_display = start_line + 1;
        if start_line >= all_lines.len() {
            return Err(AgentError::Tool(format!(
                "Offset {} is beyond end of file ({} lines total)",
                input.offset.unwrap_or(1),
                all_lines.len()
            )));
        }

        let (selected_content, user_limited_lines): (String, Option<usize>) =
            if let Some(limit) = input.limit {
                let end_line = std::cmp::min(start_line + limit as usize, all_lines.len());
                (
                    all_lines[start_line..end_line].join("\n"),
                    Some(end_line - start_line),
                )
            } else {
                (all_lines[start_line..].join("\n"), None)
            };

        let truncation = truncate_head(&selected_content, TruncationOptions::default());
        let (output_text, details): (String, Option<TruncationResult>) = if truncation
            .first_line_exceeds_limit
        {
            let first_line_size = format_size(all_lines[start_line].as_bytes().len());
            (
                    format!(
                        "[Line {start_line_display} is {first_line_size}, exceeds {} limit. \
                         Use bash: sed -n '{start_line_display}p' {} | head -c {DEFAULT_MAX_BYTES}]",
                        format_size(DEFAULT_MAX_BYTES),
                        input.path
                    ),
                    Some(truncation),
                )
        } else if truncation.truncated {
            let end_line_display = start_line_display + truncation.output_lines - 1;
            let next_offset = end_line_display + 1;
            let mut text = truncation.content.clone();
            match truncation.truncated_by {
                Some(crate::truncate::TruncationLimit::Lines) => {
                    text.push_str(&format!(
                        "\n\n[Showing lines {start_line_display}-{end_line_display} of \
                             {total_file_lines}. Use offset={next_offset} to continue.]"
                    ));
                }
                _ => {
                    text.push_str(&format!(
                        "\n\n[Showing lines {start_line_display}-{end_line_display} of \
                             {total_file_lines} ({} limit). Use offset={next_offset} to continue.]",
                        format_size(DEFAULT_MAX_BYTES)
                    ));
                }
            }
            (text, Some(truncation))
        } else if let Some(ull) = user_limited_lines {
            if start_line + ull < all_lines.len() {
                let remaining = all_lines.len() - (start_line + ull);
                let next_offset = start_line + ull + 1;
                (
                        format!(
                            "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
                            truncation.content
                        ),
                        None,
                    )
            } else {
                (truncation.content, None)
            }
        } else {
            (truncation.content, None)
        };

        let details_value = match details {
            Some(t) => serde_json::to_value(ReadToolDetails {
                truncation: Some(t),
            })
            .unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        };
        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(output_text)],
            details: details_value,
            ..Default::default()
        })
    }
}

/// The backend has already applied offset. The sentinel proves only that more
/// content exists; neither its length nor truncation totals are whole-file
/// metadata. EOF (including an offset beyond EOF) is a valid empty range.
async fn client_text_result(
    client: &FileIOSnapshot,
    absolute_path: &str,
    input: &ReadInput,
    signal: &CancellationToken,
) -> Result<AgentToolResult, AgentError> {
    let offset = input.offset.unwrap_or(1).max(1);
    let display_lines = input
        .limit
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_MAX_LINES)
        .min(DEFAULT_MAX_LINES);
    let text = client
        .read_text(
            absolute_path,
            FileIOReadRange::Lines {
                offset_1based: offset,
                limit: display_lines as u32 + 1,
            },
            signal,
        )
        .await
        .map_err(|e| file_err_to_agent(e.into()))?;
    let lines = crate::truncate::split_lines_for_counting(&text);
    let has_sentinel = lines.len() > display_lines;
    let selected = if has_sentinel {
        let selected_lines = &lines[..display_lines];
        let mut selected = selected_lines.join("\n");
        // A final blank source line needs its terminator to remain countable.
        // Otherwise a one-blank-line page becomes empty and never advances.
        if selected_lines.last() == Some(&"") {
            selected.push('\n');
        }
        selected
    } else {
        text
    };
    let truncation = truncate_head(
        &selected,
        TruncationOptions {
            max_lines: Some(display_lines),
            max_bytes: Some(DEFAULT_MAX_BYTES),
        },
    );
    let mut output = truncation.content.clone();
    if truncation.first_line_exceeds_limit {
        output = format!(
            "[Line {offset} exceeds {} display limit. Client text was not displayed.]",
            format_size(DEFAULT_MAX_BYTES)
        );
    } else if has_sentinel || truncation.truncated {
        let next = u64::from(offset) + truncation.output_lines as u64;
        output.push_str(&format!("\n\n[More content may remain; total line count unknown. Use offset={next} to continue.]"));
    }
    // Native TruncationResult.total_lines describes its bounded input, not the
    // client file. Do not serialize it as misleading file metadata.
    Ok(AgentToolResult {
        content: vec![TextContentOrImage::text(output)],
        details: serde_json::Value::Null,
        ..Default::default()
    })
}

/// Build the image-path result, honoring an optional processor. Mirrors the TS
/// image branch.
fn image_result(
    mime: &str,
    bytes: &[u8],
    options: &ReadToolOptions,
    processor: Option<&dyn ReadImageProcessor>,
) -> AgentToolResult {
    if let Some(proc) = processor {
        match proc.process(bytes, mime, options.auto_resize_images) {
            Ok(p) => {
                let hints = if p.hints.is_empty() {
                    String::new()
                } else {
                    format!("\n{}", p.hints.join("\n"))
                };
                return AgentToolResult {
                    content: vec![
                        TextContentOrImage::text(format!(
                            "Read image file [{}]{}",
                            p.mime_type, hints
                        )),
                        TextContentOrImage::Image(ImageContent {
                            kind: ImageContentType,
                            data: p.data.into(),
                            mime_type: p.mime_type,
                        }),
                    ],
                    details: serde_json::Value::Null,
                    ..Default::default()
                };
            }
            Err(message) => {
                return AgentToolResult {
                    content: vec![TextContentOrImage::text(format!(
                        "Read image file [{mime}]\n{message}"
                    ))],
                    details: serde_json::Value::Null,
                    ..Default::default()
                };
            }
        }
    }
    // No processor — BMP is omitted; others are inlined.
    if mime == "image/bmp" {
        return AgentToolResult {
            content: vec![TextContentOrImage::text(
                "Read image file [image/bmp]\n[Image omitted: configure an imageProcessor to convert BMP images.]",
            )],
            details: serde_json::Value::Null,
            ..Default::default()
        };
    }
    AgentToolResult {
        content: vec![
            TextContentOrImage::text(format!("Read image file [{mime}]")),
            TextContentOrImage::Image(ImageContent {
                kind: ImageContentType,
                data: encode_base64(bytes).into(),
                mime_type: mime.to_string(),
            }),
        ],
        details: serde_json::Value::Null,
        ..Default::default()
    }
}

/// Map a `FileError` into an `AgentError::Tool`, mirroring the TS `getOrThrow`
/// surface (the message carries the path + code).
fn file_err_to_agent(e: crate::error::FileError) -> AgentError {
    AgentError::Tool(e.to_string())
}
