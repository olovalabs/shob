//! Native-owned external tool execution; no backend protocol lives here.
use super::{
    client_requests::{ClientRequestBroker, MAX_TIMEOUT_MS},
    writer::RpcWriter,
};
use async_trait::async_trait;
use rpi_agent::{
    error::AgentError,
    types::{AgentToolResult, TextContentOrImage},
};
use rpi_ai::types::Content;
use rpi_harness::agent_harness::AgentLane;
use rpi_tools::tools::external::ExternalToolBackend;
use serde_json::{json, Value};
use std::{
    sync::{Arc, Weak},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub(crate) struct ExternalTools {
    pub broker: ClientRequestBroker,
    writer: std::sync::Mutex<Option<RpcWriter>>,
    stop: CancellationToken,
    // The RPC owner retains the strong lane; registry -> backend must not cycle.
    lane: Option<Weak<dyn AgentLane>>,
}
impl ExternalTools {
    pub fn new(writer: RpcWriter, lane: Weak<dyn AgentLane>) -> Arc<Self> {
        Self::with_lane(writer, Some(lane))
    }
    #[cfg(test)]
    pub fn detached(writer: RpcWriter) -> Arc<Self> {
        Self::with_lane(writer, None)
    }
    fn with_lane(writer: RpcWriter, lane: Option<Weak<dyn AgentLane>>) -> Arc<Self> {
        Arc::new(Self {
            broker: ClientRequestBroker::default(),
            writer: std::sync::Mutex::new(Some(writer)),
            stop: CancellationToken::new(),
            lane,
        })
    }
    async fn abort_foreground(&self) {
        if let Some(lane) = self.lane.as_ref().and_then(Weak::upgrade) {
            let _ = lane.abort().await;
        }
    }
    pub fn abort(&self) {
        self.broker.cancel_all();
    }
    pub fn shutdown(&self) {
        self.stop.cancel();
        self.abort();
        // Registry entries can outlive RPC. They must not keep its writer open.
        self.writer.lock().unwrap().take();
    }
    pub fn respond(&self, message: &Value) -> Result<bool, String> {
        let id = message
            .get("requestId")
            .and_then(Value::as_str)
            .ok_or("external_tool_response requires requestId")?;
        if message
            .get("cancelled")
            .is_some_and(|value| value != &Value::Bool(true))
        {
            return Err("cancelled must be true when present".into());
        }
        let forms = usize::from(message.get("result").is_some())
            + usize::from(message.get("error").is_some())
            + usize::from(message.get("cancelled") == Some(&Value::Bool(true)));
        if forms != 1 {
            return Err(
                "external_tool_response requires exactly one result, error, or cancelled:true"
                    .into(),
            );
        }
        let reply = if message.get("cancelled") == Some(&Value::Bool(true)) {
            Err("external tool cancelled".into())
        } else if let Some(error) = message.get("error") {
            Err(error
                .as_str()
                .ok_or("external error must be a string")?
                .to_owned())
        } else {
            let result = message["result"].clone();
            validate_result(&result).map_err(|e| e.to_string())?;
            Ok(result)
        };
        Ok(self.broker.resolve(id, reply))
    }
}

// Preserve the complete unmodified result alongside its model-facing native
// content. Unknown backend metadata is never truncated or converted to text.
fn validate_result(raw: &Value) -> Result<(), AgentError> {
    if raw.get("isError").is_some_and(|value| !value.is_boolean()) {
        return Err(AgentError::Validation(
            "external result isError must be boolean".into(),
        ));
    }
    if raw
        .get("structuredContent")
        .is_some_and(|value| !value.is_object())
    {
        return Err(AgentError::Validation(
            "external result structuredContent must be an object".into(),
        ));
    }
    decode_content(raw).map(|_| ())
}

fn decode_content(raw: &Value) -> Result<Vec<TextContentOrImage>, AgentError> {
    let blocks = raw
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| AgentError::Validation("external result requires a content array".into()))?;
    let mut content = Vec::with_capacity(blocks.len());
    for block in blocks {
        let parsed: Content = serde_json::from_value(block.clone())
            .map_err(|e| AgentError::Validation(e.to_string()))?;
        content.push(match parsed {
            Content::Text(mut t) => {
                // Signatures and annotations are controller metadata, not
                // provider authority. Keep them only in the raw details.
                t.text_signature = None;
                TextContentOrImage::Text(t)
            }
            Content::Image(i) => {
                use base64::Engine;
                if !i.mime_type.starts_with("image/")
                    || i.mime_type.len() <= 6
                    || base64::engine::general_purpose::STANDARD
                        .decode(&*i.data)
                        .is_err()
                {
                    return Err(AgentError::Validation(
                        "external image requires image MIME type and base64 data".into(),
                    ));
                }
                TextContentOrImage::Image(i)
            }
            _ => {
                return Err(AgentError::Validation(
                    "external tool content must be native text or image blocks".into(),
                ));
            }
        });
    }
    Ok(content)
}

fn decode_result(raw: Value) -> Result<AgentToolResult, AgentError> {
    validate_result(&raw)?;
    let mut content = decode_content(&raw)?;
    if raw.get("isError") == Some(&Value::Bool(true)) {
        // AgentToolResult has no error flag: the native execution boundary
        // carries errors through AgentError, never through a successful result.
        // AgentError has no details channel. Only explicit result content is
        // projected on errors; unknown raw metadata must not leak to providers.
        let blocks = content
            .iter()
            .map(|block| match block {
                TextContentOrImage::Text(text) => json!({"type":"text", "text":text.text}),
                TextContentOrImage::Image(image) => {
                    json!({"type":"image", "data":image.data, "mimeType":image.mime_type})
                }
            })
            .collect::<Vec<_>>();
        let mut error = json!({"content": blocks, "isError": true});
        if let Some(structured) = raw.get("structuredContent") {
            error["structuredContent"] = structured.clone();
        }
        return Err(AgentError::Tool(error.to_string()));
    }
    if let Some(structured) = raw.get("structuredContent") {
        // Details do not reach providers. Project only the explicit structured
        // result, never backend metadata, and avoid repeating equivalent JSON.
        let represented = content.iter().any(|block| match block {
            TextContentOrImage::Text(text) => {
                serde_json::from_str::<Value>(&text.text).is_ok_and(|value| value == *structured)
            }
            _ => false,
        });
        if !represented {
            content.push(TextContentOrImage::text(structured.to_string()));
        }
    }
    Ok(AgentToolResult {
        content,
        details: json!({"externalToolResult":raw}),
        ..Default::default()
    })
}

#[async_trait]
impl ExternalToolBackend for ExternalTools {
    async fn execute(
        &self,
        name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> Result<AgentToolResult, AgentError> {
        if cancel.is_cancelled() || self.stop.is_cancelled() {
            return Err(AgentError::Abort);
        }
        // Child requests have no trusted session routing in the external bridge.
        // Host task-local provenance, never caller arguments, controls this gate.
        // Fail before allocating a request or emitting any external event.
        if rpi_agent::tool_execution_scope::is_child_execution() {
            return Err(AgentError::Tool(
                "external tools are unsupported in child agents: the external bridge does not support child-session routing".into(),
            ));
        }
        let writer = self
            .writer
            .lock()
            .unwrap()
            .clone()
            .ok_or(AgentError::Abort)?;
        let request = self.broker.register().map_err(AgentError::Tool)?;
        let mut close = CloseRequest {
            writer: writer.clone(),
            id: request.id.clone(),
            closed: false,
        };
        let event = json!({"type":"external_tool_request", "requestId":request.id, "toolName":name, "arguments":arguments});
        if event.to_string().len() + 1 > super::protocol::MAX_FRAME_BYTES {
            close.close();
            if writer.failure().is_cancelled() {
                self.abort_foreground().await;
            }
            return Err(AgentError::Validation(
                "external arguments exceed frame limit".into(),
            ));
        }
        let failure = writer.failure();
        let operation = async {
            if writer.write_bounded(&event).await.is_err() {
                // Signal the ROOT, not the tool's child token, before this
                // operation drops its captured request or returns to the loop.
                self.abort_foreground().await;
                return Err(AgentError::Abort);
            }
            request
                .wait(Duration::from_millis(MAX_TIMEOUT_MS), &cancel)
                .await
                .map_err(AgentError::Tool)
        };
        // Keep the losing operation alive until the failure branch has awaited
        // root cancellation; select's temporary futures otherwise drop too soon.
        let operation = tokio::time::timeout(Duration::from_millis(MAX_TIMEOUT_MS), operation);
        tokio::pin!(operation);
        let raw = tokio::select! {
            biased;
            _ = self.stop.cancelled() => Err(AgentError::Abort),
            _ = cancel.cancelled() => Err(AgentError::Abort),
            _ = failure.cancelled() => {
                self.abort_foreground().await;
                Err(AgentError::Abort)
            },
            result = &mut operation => result.unwrap_or_else(|_| Err(AgentError::Tool("external tool timed out".into()))),
        };
        // Closing a successful or failed request can itself overflow stdout.
        // Do it explicitly before returning; Drop is cancellation-only fallback.
        close.close();
        if failure.is_cancelled() {
            self.abort_foreground().await;
            return Err(AgentError::Abort);
        }
        decode_result(raw?)
    }
}
struct CloseRequest {
    writer: RpcWriter,
    id: String,
    closed: bool,
}
impl CloseRequest {
    fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            let event = json!({"type":"external_tool_request_closed", "requestId":self.id});
            self.writer.try_write_line(&event);
        }
    }
}
impl Drop for CloseRequest {
    fn drop(&mut self) {
        // Best effort for externally cancelled futures. Normal exits close
        // explicitly and await foreground abort on authoritative queue failure.
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_result_is_preserved_and_unsupported_content_rejected() {
        let raw = json!({"content":[{"type":"text","text":"a","annotations":{"x":1}},{"type":"image","data":"AA==","mimeType":"image/png"}],"structuredContent":{"x":2}});
        let result = decode_result(raw.clone()).unwrap();
        assert_eq!(result.content.len(), 3);
        assert_eq!(result.content[2], TextContentOrImage::text(r#"{"x":2}"#));
        assert_eq!(result.details["externalToolResult"], raw);
        assert!(decode_result(json!({"content":[{"type":"resource"}]})).is_err());
    }

    include!("external_tools_result_tests.rs");
    include!("external_tools_broker_tests.rs");
    include!("external_tools_child_tests.rs");
    include!("external_tools_failure_ordering_tests.rs");
}
