//! `tool_result` transform — a plugin's chance to change what a finished tool
//! call hands the model.
//!
//! pi lets a `tool_result` handler return a changed `content`/`details`
//! (`ToolResultEventResult`), and the agent records that instead of what the
//! tool produced. rpi broadcasts [`EventTag::ToolResult`] to observers only;
//! this module carries the return channel, the way
//! [`prompt_transform`](crate::prompt_transform) does for `before_agent_start`.
//!
//! - **Chained in registration order.** Each handler sees the result the
//!   previous one returned.
//! - **Partial replacement.** A handler returns only the fields it changes,
//!   `content`, `details` or both; anything else (`terminate`, usage, added
//!   tools) stays the tool's.
//! - **A failing handler is skipped, not fatal.** Nonzero `rc`, a panic or
//!   unreadable output leave the result as the previous handler left it.
//! - **Only when registered.** Tools are wrapped only when at least one handler
//!   exists, so a session without such a plugin runs the tools untouched.
//!
//! [`EventTag::ToolResult`]: rpi_plugin_sdk::EventTag::ToolResult

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolExecutionMode, ToolResultPartial};
use rpi_agent::{AgentError, AgentTool};
use rpi_ai::types::{ImageContent, ImageContentType, Tool};
use rpi_plugin_sdk::{StbString, StbStringRef};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::loader::PluginKeepalive;
use crate::registry::{assert_active, RegistrySnapshot, ToolResultHandler};

/// Wrap `tool` so its successful results pass through the snapshot's
/// `tool_result` transforms. Returns `tool` itself when there are none.
pub fn with_tool_result_transform(
    tool: Arc<dyn AgentTool>,
    snapshot: Arc<RegistrySnapshot>,
    keepalive: Arc<PluginKeepalive>,
) -> Arc<dyn AgentTool> {
    if snapshot.tool_result().is_empty() {
        return tool;
    }
    Arc::new(TransformedTool {
        tool,
        snapshot,
        keepalive,
    })
}

/// Run every `tool_result` transform over one result. `None` means no handler
/// changed anything (or the snapshot is stale), so the caller keeps `result`.
pub fn emit_tool_result(
    tool_call_id: &str,
    tool_name: &str,
    input: &Value,
    result: &AgentToolResult,
    snapshot: &RegistrySnapshot,
) -> Option<AgentToolResult> {
    if !assert_active(snapshot.active_flag()) {
        return None;
    }
    let handlers = snapshot.tool_result();
    if handlers.is_empty() {
        return None;
    }
    let mut content = content_to_json(&result.content);
    let mut details = result.details.clone();
    let mut changed = false;
    let mut content_replaced = false;
    for h in handlers {
        let envelope = json!({
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "input": input,
            "content": content,
            "details": details,
        })
        .to_string();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            call_one_handler(*h, StbStringRef::from_str(&envelope))
        }));
        match outcome {
            Ok(Ok(Some(patch))) => {
                if let Some(next) = patch.get("content").filter(|v| v.is_array()) {
                    content = next.clone();
                    content_replaced = true;
                    changed = true;
                }
                if let Some(next) = patch.get("details") {
                    details = next.clone();
                    changed = true;
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(rc)) => tracing::warn!(
                rc,
                tool_name,
                "tool_result handler returned nonzero — result left as-is (fan-out continues)"
            ),
            Err(_) => tracing::error!(
                tool_name,
                "tool_result handler panicked — result left as-is (fan-out continues)"
            ),
        }
    }
    changed.then(|| AgentToolResult {
        content: if content_replaced {
            content_from_json(&content)
        } else {
            result.content.clone()
        },
        details,
        ..result.clone()
    })
}

struct TransformedTool {
    tool: Arc<dyn AgentTool>,
    snapshot: Arc<RegistrySnapshot>,
    keepalive: Arc<PluginKeepalive>,
}

#[async_trait]
impl AgentTool for TransformedTool {
    fn schema(&self) -> &Tool {
        self.tool.schema()
    }
    fn label(&self) -> &str {
        self.tool.label()
    }
    fn execution_mode(&self) -> ToolExecutionMode {
        self.tool.execution_mode()
    }
    fn prepare_arguments(&self, args: Value) -> Result<Value, AgentError> {
        self.tool.prepare_arguments(args)
    }
    async fn execute(
        &self,
        id: &str,
        params: Value,
        signal: CancellationToken,
        on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let result = self
            .tool
            .execute(id, params.clone(), signal, on_update)
            .await?;
        let snapshot = Arc::clone(&self.snapshot);
        let keepalive = Arc::clone(&self.keepalive);
        let id = id.to_string();
        let name = self.tool.schema().name.clone();
        // Handlers may be slow (re-encoding an image), so keep them off the
        // async workers. A join error keeps the tool's own result.
        tokio::task::spawn_blocking(move || {
            // Dropping the awaiter cannot stop a started blocking callback.
            // Keep its library loaded through handler execution and the
            // producer-side reclamation of every returned string.
            let _keepalive = keepalive;
            match emit_tool_result(&id, &name, &params, &result, &snapshot) {
                Some(changed) => changed,
                None => result,
            }
        })
        .await
        .map_err(|error| AgentError::Tool(format!("tool_result transform failed: {error}")))
    }
}

/// Call one handler; `Ok(None)` is "no change" (empty or non-object output).
fn call_one_handler(h: ToolResultHandler, event: StbStringRef) -> Result<Option<Value>, i32> {
    let mut out = StbString::empty();
    let rc = (h.handler)(event, &mut out, h.user_data);
    // SAFETY: the callback transfers its live output to its registered producer.
    let out = unsafe {
        rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(h.plugin_free_string))
    };
    if rc != 0 {
        return Err(rc);
    }
    let text = out.to_string_lossy();
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    Ok(serde_json::from_str::<Value>(text)
        .ok()
        .filter(Value::is_object))
}

/// Result content in the shape a plugin tool returns it.
fn content_to_json(content: &[TextContentOrImage]) -> Value {
    serde_json::to_value(content).expect("tool result content serializes to JSON")
}

/// The inverse of [`content_to_json`], as lenient as plugin tool results: an
/// unknown block type is read as text, a missing `mimeType` as PNG.
fn content_from_json(content: &Value) -> Vec<TextContentOrImage> {
    let field = |block: &Value, key: &str| {
        block
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .map(|block| match block.get("type").and_then(Value::as_str) {
                    Some("image") => TextContentOrImage::Image(
                        serde_json::from_value(block.clone()).unwrap_or_else(|_| ImageContent {
                            kind: ImageContentType,
                            data: field(block, "data").into(),
                            mime_type: block
                                .get("mimeType")
                                .and_then(Value::as_str)
                                .unwrap_or("image/png")
                                .to_string(),
                        }),
                    ),
                    _ => serde_json::from_value(block.clone())
                        .map(TextContentOrImage::Text)
                        .unwrap_or_else(|_| TextContentOrImage::text(field(block, "text"))),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ExtensionRegistry;
    use rpi_plugin_sdk::callbacks::free_string;

    /// Swaps every PNG image for a "webp" one and tags `details`.
    extern "C" fn to_webp(
        event: StbStringRef,
        out: *mut StbString,
        _user_data: *mut std::ffi::c_void,
    ) -> i32 {
        // SAFETY: the host keeps the envelope alive for the call.
        let event: Value = serde_json::from_str(unsafe { event.as_str() }).unwrap();
        let mut content = event["content"].clone();
        for block in content.as_array_mut().unwrap() {
            if block["mimeType"] == "image/png" {
                block["mimeType"] = json!("image/webp");
                block["data"] = json!("V0VCUA==");
            }
        }
        let patch = json!({"content": content, "details": {"seen": event["toolName"]}});
        // SAFETY: `out` is the host's slot for this call.
        unsafe { *out = StbString::from_string(patch.to_string()) };
        0
    }

    /// Sees the previous handler's output and appends a text block.
    extern "C" fn append_note(
        event: StbStringRef,
        out: *mut StbString,
        _user_data: *mut std::ffi::c_void,
    ) -> i32 {
        // SAFETY: the host keeps the envelope alive for the call.
        let event: Value = serde_json::from_str(unsafe { event.as_str() }).unwrap();
        let mut content = event["content"].clone();
        let mime = content[1]["mimeType"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        content
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "text", "text": format!("after {mime}")}));
        // SAFETY: as above.
        unsafe { *out = StbString::from_string(json!({"content": content}).to_string()) };
        0
    }

    extern "C" fn fails(_: StbStringRef, _: *mut StbString, _: *mut std::ffi::c_void) -> i32 {
        3
    }

    fn snapshot(handlers: &[rpi_plugin_sdk::ToolResultFn]) -> Arc<RegistrySnapshot> {
        let mut registry = ExtensionRegistry::new();
        for h in handlers {
            registry.register_tool_result(*h, free_string, std::ptr::null_mut());
        }
        Arc::new(registry.snapshot())
    }

    fn result() -> AgentToolResult {
        AgentToolResult {
            content: vec![
                TextContentOrImage::text("Read image file [image/png]"),
                TextContentOrImage::Image(ImageContent {
                    kind: ImageContentType,
                    data: "UE5H".into(),
                    mime_type: "image/png".into(),
                }),
            ],
            details: json!({"path": "a.png"}),
            terminate: true,
            ..Default::default()
        }
    }

    #[test]
    fn handlers_chain_and_replace_only_what_they_return() {
        let snap = snapshot(&[fails, to_webp, append_note]);
        let changed = emit_tool_result("c1", "read", &json!({"path": "a.png"}), &result(), &snap)
            .expect("changed");
        let TextContentOrImage::Image(image) = &changed.content[1] else {
            panic!("image kept its place")
        };
        assert_eq!(
            (image.mime_type.as_str(), &*image.data),
            ("image/webp", "V0VCUA==")
        );
        assert_eq!(
            changed.content[2],
            TextContentOrImage::text("after image/webp")
        );
        assert_eq!(changed.details, json!({"seen": "read"}));
        assert!(
            changed.terminate,
            "fields a handler cannot return stay the tool's"
        );
    }

    #[test]
    fn patches_preserve_content_metadata() {
        extern "C" fn patch_details(
            event: StbStringRef,
            out: *mut StbString,
            _: *mut std::ffi::c_void,
        ) -> i32 {
            // SAFETY: the envelope and output slot are live for the callback.
            let event: Value = serde_json::from_str(unsafe { event.as_str() }).unwrap();
            let mut patch = json!({"details": {"updated": true}});
            if event["input"]["replaceContent"] == true {
                patch["content"] = event["content"].clone();
            }
            unsafe { *out = StbString::from_string(patch.to_string()) };
            0
        }

        let mut original = result();
        let TextContentOrImage::Text(text) = &mut original.content[0] else {
            unreachable!()
        };
        text.text_signature = Some("stable-signature".into());
        let snap = snapshot(&[patch_details]);
        // Both a details-only patch and an explicit identity content replacement
        // must preserve all supported content metadata.
        for replace_content in [false, true] {
            let changed = emit_tool_result(
                "c1",
                "read",
                &json!({"replaceContent": replace_content}),
                &original,
                &snap,
            )
            .expect("details changed");
            assert_eq!(changed.details, json!({"updated": true}));
            assert_eq!(changed.content, original.content);
        }
    }

    #[tokio::test]
    async fn aborted_awaiter_keeps_blocking_handler_library_alive() {
        use std::sync::{mpsc, Mutex};
        use std::time::Duration;

        struct Paused {
            entered: tokio::sync::Notify,
            release: Mutex<mpsc::Receiver<()>>,
        }
        extern "C" fn paused(
            _: StbStringRef,
            out: *mut StbString,
            user_data: *mut std::ffi::c_void,
        ) -> i32 {
            // SAFETY: the test retains the state until the blocking task exits.
            let state = unsafe { &*user_data.cast::<Paused>() };
            state.entered.notify_one();
            if state
                .release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .is_err()
            {
                return 1;
            }
            // Exercise producer-side output reclamation as well as the callback.
            unsafe { *out = StbString::from_string(r#"{"details":{"updated":true}}"#.into()) };
            0
        }

        // No plugin adapter: only the transform can retain the library here.
        struct BuiltinTool(Tool);
        #[async_trait]
        impl AgentTool for BuiltinTool {
            fn schema(&self) -> &Tool {
                &self.0
            }
            fn label(&self) -> &str {
                "read"
            }
            async fn execute(
                &self,
                _: &str,
                _: Value,
                _: CancellationToken,
                _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
            ) -> Result<AgentToolResult, AgentError> {
                Ok(result())
            }
        }

        let (release, receiver) = mpsc::channel();
        let state = Arc::new(Paused {
            entered: tokio::sync::Notify::new(),
            release: Mutex::new(receiver),
        });
        let mut registry = ExtensionRegistry::new();
        registry.register_tool_result(
            paused,
            free_string,
            Arc::as_ptr(&state) as *mut std::ffi::c_void,
        );
        let session = crate::ExtensionSession::from_parts(
            Arc::new(registry.snapshot()),
            PluginKeepalive::empty(),
            Vec::new(),
            None,
        );
        let weak = Arc::downgrade(&session.keepalive());
        let tool = with_tool_result_transform(
            Arc::new(BuiltinTool(Tool {
                name: "read".into(),
                description: "test built-in".into(),
                parameters: json!({"type": "object"}).into(),
                constrained_sampling: None,
            })),
            session.snapshot_arc().unwrap(),
            session.keepalive(),
        );
        let task = tokio::spawn(async move {
            tool.execute(
                "c1",
                Value::Null,
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), state.entered.notified())
            .await
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        registry.invalidate();
        drop(session);
        let retained = weak.upgrade().is_some();
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            retained,
            "aborting the awaiter must not unload its running handler"
        );
    }

    #[test]
    fn no_handler_or_no_change_leaves_the_tool_alone() {
        assert!(emit_tool_result("c1", "read", &Value::Null, &result(), &snapshot(&[])).is_none());
        assert!(
            emit_tool_result("c1", "read", &Value::Null, &result(), &snapshot(&[fails])).is_none()
        );
    }
}
