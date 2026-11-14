//! Ordered prompt preflight used by the production RPC dispatcher.
use rpi_ai::Content;
use serde_json::{json, Value};
use std::io::{self, Write};

pub const MAX_CONTENT_BLOCKS: usize = 64;
pub const MAX_IMAGES: usize = 4;
pub const MAX_TOTAL_IMAGE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_TEXT_BYTES: usize = 32 * 1024;
pub const MAX_PROMPT_BYTES: usize = 3 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedPrompt {
    Legacy(String),
    Blocks(Vec<Content>),
}

impl ParsedPrompt {
    pub fn has_images(&self) -> bool {
        matches!(self, Self::Blocks(blocks) if blocks.iter().any(|b| matches!(b, Content::Image(_))))
    }
}

/// Cheap structural validation for the public serde schema. No image decode,
/// URI lookup, filesystem access or model/session mutation occurs here.
pub fn validate_prompt_shape(command: &Value) -> Result<(), String> {
    let object = command
        .as_object()
        .ok_or("prompt command must be an object")?;
    if object.contains_key("images") {
        return Err("prompt `images` is unsupported; use ordered `contentBlocks`".into());
    }
    let Some(value) = object.get("contentBlocks") else {
        return object
            .get("message")
            .or_else(|| object.get("content"))
            .or_else(|| object.get("prompt"))
            .and_then(Value::as_str)
            .map(|_| ())
            .ok_or_else(|| "prompt requires a string `message`".into());
    };
    if ["message", "content", "prompt"]
        .iter()
        .any(|key| object.contains_key(*key))
    {
        return Err("contentBlocks cannot be combined with message/content/prompt".into());
    }
    let blocks = value
        .as_array()
        .filter(|b| !b.is_empty() && b.len() <= MAX_CONTENT_BLOCKS)
        .ok_or("contentBlocks must contain 1..64 blocks")?;
    for (index, block) in blocks.iter().enumerate() {
        let valid = block.as_object().is_some_and(|fields| {
            match fields.get("type").and_then(Value::as_str) {
                Some("text") => {
                    fields.len() == 2 && fields.get("text").is_some_and(Value::is_string)
                }
                Some("image") => {
                    fields.len() == 3
                        && fields.get("data").is_some_and(Value::is_string)
                        && fields.get("mimeType").is_some_and(Value::is_string)
                }
                _ => false,
            }
        });
        if !valid {
            return Err(format!(
                "contentBlocks[{index}]: invalid plain text/image block"
            ));
        }
    }
    Ok(())
}

/// Validate the entire command (including id and extension fields). This performs
/// synchronous pixel decoding: the caller must offload and own the worker before
/// admission. It does not check model support or mutate lane/session/run state.
pub fn parse_prompt_content(command: &Value) -> Result<ParsedPrompt, String> {
    #[cfg(test)]
    let _worker = validation_test_hook::for_command(command).map(|gate| gate.enter_worker());
    let object = command
        .as_object()
        .ok_or_else(|| "prompt command must be an object".to_string())?;
    if object.contains_key("images") {
        return Err("prompt `images` is unsupported; use ordered `contentBlocks`".into());
    }
    let Some(value) = object.get("contentBlocks") else {
        // Preserve the actual dispatcher precedence: first PRESENT alias wins;
        // a non-string message does not fall back to a string content/prompt.
        return object
            .get("message")
            .or_else(|| object.get("content"))
            .or_else(|| object.get("prompt"))
            .and_then(Value::as_str)
            .map(|s| ParsedPrompt::Legacy(s.to_owned()))
            .ok_or_else(|| "prompt requires a string `message`".to_string());
    };
    if ["message", "content", "prompt"]
        .iter()
        .any(|key| object.contains_key(*key))
    {
        return Err("contentBlocks cannot be combined with message/content/prompt".into());
    }
    let blocks = value
        .as_array()
        .filter(|b| !b.is_empty() && b.len() <= MAX_CONTENT_BLOCKS)
        .ok_or_else(|| "contentBlocks must contain 1..64 blocks".to_string())?;
    validate_command_size(command)?;

    let mut text_bytes = 0usize;
    let mut image_count = 0usize;
    let mut image_bytes = 0usize;
    // Shape/count/budget checks precede pixel work. Only the exact plain wire
    // fields are allowed; provider signatures/URI/metadata are not user input.
    for (index, block) in blocks.iter().enumerate() {
        let mut check = || -> Result<(), String> {
            let fields = block.as_object().ok_or("block must be an object")?;
            match fields.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if fields.len() != 2 || !fields.contains_key("text") {
                        return Err("text block permits only type and text".into());
                    }
                    let text = fields["text"].as_str().ok_or("text must be a string")?;
                    text_bytes = text_bytes
                        .checked_add(text.len())
                        .ok_or("text budget overflow")?;
                    if text_bytes > MAX_TEXT_BYTES {
                        return Err("aggregate text exceeds 32768 UTF-8 bytes".into());
                    }
                }
                Some("image") => {
                    if fields.len() != 3
                        || !fields.contains_key("data")
                        || !fields.contains_key("mimeType")
                    {
                        return Err("image block permits only type, data and mimeType".into());
                    }
                    let data = fields["data"]
                        .as_str()
                        .ok_or("image data must be a string")?;
                    let mime = fields["mimeType"]
                        .as_str()
                        .ok_or("image MIME must be a string")?;
                    if !matches!(mime, "image/png" | "image/jpeg") {
                        return Err("image MIME must be image/png or image/jpeg".into());
                    }
                    image_count += 1;
                    if image_count > MAX_IMAGES {
                        return Err("prompt exceeds 4 images".into());
                    }
                    // Strict canonical decode here supplies an exact byte budget
                    // without entering any image decoder on over-budget requests.
                    use base64::{engine::general_purpose::STANDARD, Engine};
                    use rpi_tools::image_processing::{
                        RPC_IMAGE_MAX_BASE64_BYTES, RPC_IMAGE_MAX_BYTES,
                    };
                    if data.is_empty() || data.len() > RPC_IMAGE_MAX_BASE64_BYTES {
                        return Err(format!(
                            "image data must contain 1..{} decoded bytes",
                            RPC_IMAGE_MAX_BYTES
                        ));
                    }
                    let bytes = STANDARD
                        .decode(data)
                        .map_err(|_| "image data must be canonical standard base64")?;
                    if bytes.is_empty() || bytes.len() > RPC_IMAGE_MAX_BYTES {
                        return Err(format!(
                            "image data must contain 1..{} decoded bytes",
                            RPC_IMAGE_MAX_BYTES
                        ));
                    }
                    if STANDARD.encode(&bytes) != data {
                        return Err("image data must be canonical standard base64".into());
                    }
                    image_bytes = image_bytes
                        .checked_add(bytes.len())
                        .ok_or("image budget overflow")?;
                    if image_bytes > MAX_TOTAL_IMAGE_BYTES {
                        return Err(format!(
                            "aggregate image data exceeds {} decoded bytes",
                            MAX_TOTAL_IMAGE_BYTES
                        ));
                    }
                }
                _ => return Err("block type must be text or image".into()),
            }
            Ok(())
        };
        check().map_err(|e| format!("contentBlocks[{index}]: {e}"))?;
    }
    let mut content = Vec::with_capacity(blocks.len());
    for (index, block) in blocks.iter().enumerate() {
        if block["type"] == "image" {
            rpi_tools::image_processing::validate_rpc_inline_image(
                block["data"].as_str().expect("validated image data"),
                block["mimeType"].as_str().expect("validated image MIME"),
            )
            .map_err(|e| format!("contentBlocks[{index}]: {e}"))?;
        }
        // Reuse the native representation only AFTER restricting the input
        // fields; Content's provider-side variants are intentionally broader.
        content.push(
            serde_json::from_value::<Content>(block.clone())
                .map_err(|_| format!("contentBlocks[{index}]: invalid plain content block"))?,
        );
    }
    Ok(ParsedPrompt::Blocks(content))
}

// Exact serde JSON bytes plus LF, without allocating a second command-sized
// buffer or cloning a possibly large id. Stop writing at the first excess byte.
fn validate_command_size(command: &Value) -> Result<(), String> {
    struct Budget(usize);
    impl Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .filter(|n| *n <= MAX_PROMPT_BYTES - 1)
                .ok_or_else(|| io::Error::other("prompt frame budget exceeded"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(0), command).map_err(|_| {
        format!(
            "serialized contentBlocks command plus LF exceeds {} bytes",
            MAX_PROMPT_BYTES
        )
        .into()
    })
}

/// The versioned profile describes preflight limits, not a hard heap limit.
pub fn prompt_capabilities() -> Value {
    use rpi_tools::image_processing as image;
    json!({
        "contentBlocksVersion": 1,
        "image": true,
        "imageMimeTypes": ["image/png", "image/jpeg"],
        "maxImages": MAX_IMAGES,
        "maxImageBytes": image::RPC_IMAGE_MAX_BYTES,
        "maxTotalImageBytes": MAX_TOTAL_IMAGE_BYTES,
        "maxImageBase64Bytes": image::RPC_IMAGE_MAX_BASE64_BYTES,
        "maxPromptBytes": MAX_PROMPT_BYTES,
        "maxTextBytes": MAX_TEXT_BYTES,
        "maxContentBlocks": MAX_CONTENT_BLOCKS,
        "maxImageSide": image::INLINE_IMAGE_MAX_SIDE,
        "maxImagePixels": image::RPC_IMAGE_MAX_PIXELS,
        // Decoder accounting is best-effort, not a hard heap ceiling.
        "imageDecodeAllocBudgetBytes": image::RPC_IMAGE_MAX_ALLOC,
        "frameBytes": super::protocol::MAX_FRAME_BYTES
    })
}

/// Deterministic production-worker gate, selected by a unique test command ID.
/// No timing assumptions and no changes to non-test protocol behavior.
#[cfg(test)]
pub(crate) mod validation_test_hook {
    use super::Value;
    use std::collections::HashMap;
    use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
    use tokio_util::sync::CancellationToken;

    #[derive(Default)]
    pub(crate) struct Gate {
        pub entered: CancellationToken,
        pub worker_finished: CancellationToken,
        pub run_finished: CancellationToken,
        released: Mutex<bool>,
        release_changed: Condvar,
    }
    fn gates() -> &'static Mutex<HashMap<String, Weak<Gate>>> {
        static GATES: OnceLock<Mutex<HashMap<String, Weak<Gate>>>> = OnceLock::new();
        GATES.get_or_init(|| Mutex::new(HashMap::new()))
    }
    impl Gate {
        pub fn register(id: &str) -> Arc<Self> {
            let gate = Arc::new(Self::default());
            let mut gates = gates().lock().unwrap();
            gates.retain(|_, gate| gate.strong_count() > 0);
            assert!(gates.insert(id.to_owned(), Arc::downgrade(&gate)).is_none());
            gate
        }
        pub fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.release_changed.notify_all();
        }
        pub fn enter_worker(self: Arc<Self>) -> WorkerFinished {
            self.entered.cancel();
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.release_changed.wait(released).unwrap();
            }
            drop(released);
            WorkerFinished(self)
        }
    }
    pub(crate) fn for_command(command: &Value) -> Option<Arc<Gate>> {
        let id = command.get("id")?.as_str()?;
        gates().lock().unwrap().get(id)?.upgrade()
    }
    pub(crate) struct WorkerFinished(Arc<Gate>);
    impl Drop for WorkerFinished {
        fn drop(&mut self) {
            self.0.worker_finished.cancel();
        }
    }
    pub(crate) struct RunFinished(pub Arc<Gate>);
    impl Drop for RunFinished {
        fn drop(&mut self) {
            self.0.run_finished.cancel();
        }
    }
    // Test panic/timeout must also release the blocking decoder.
    pub(crate) struct ReleaseOnDrop(pub Arc<Gate>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }
}

#[cfg(test)]
#[path = "prompt_content_tests.rs"]
mod tests;
