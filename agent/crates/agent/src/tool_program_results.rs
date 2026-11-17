//! Invocation-owned finalized results. Request disposal is deliberately independent.
use super::*;
use tokio::sync::OwnedSemaphorePermit;

pub(super) const TREE_BYTES: usize = 64 * 1024 * 1024;
#[derive(Default)]
pub(super) struct Store(HashMap<String, Retained>);
struct Retained {
    native: crate::AgentToolResult,
    is_error: bool,
    encoded: Box<str>,
    _bytes: OwnedSemaphorePermit,
}
impl Store {
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }
}
// JSON's encoded punctuation can be much smaller than its native Value/map
// nodes. Include a conservative per-node allocation estimate, not just strings.
fn value_overhead(value: &Value) -> Option<usize> {
    let add = |sum: usize, child: &Value| sum.checked_add(value_overhead(child)?);
    match value {
        Value::Array(values) => values.iter().try_fold(128, add),
        Value::Object(values) => values.values().try_fold(128, add),
        _ => Some(128),
    }
}
impl ToolProgram {
    pub(super) fn retain_result(
        &self,
        id: &str,
        native: crate::AgentToolResult,
        is_error: bool,
        encoded: String,
    ) -> Result<Value, String> {
        // Charge encoded bytes plus a native payload estimate and structural overhead.
        // The shared permit follows ownership and is returned on release/finish/drop.
        let charge = encoded
            .len()
            .checked_mul(2)
            .and_then(|n| n.checked_add(4096))
            .and_then(|n| n.checked_add(value_overhead(&native.details)?))
            .and_then(|n| n.checked_add(native.content.len().checked_mul(256)?))
            .filter(|n| *n <= TREE_BYTES)
            .ok_or("ToolProgram result reference byte quota exceeded")?;
        let mut state = self.state.lock().unwrap();
        if state.results.0.len() >= 16 {
            return Err("ToolProgram retained result reference limit exceeded".into());
        }
        let bytes = self
            .result_bytes
            .clone()
            .try_acquire_many_owned(charge as u32)
            .map_err(|_| "ToolProgram invocation-tree result reference byte quota exceeded")?;
        let manifest = json!({"requestId":id,"status":"done","isError":is_error,
            "resultReference":{"version":1,"id":id,"byteLength":encoded.len(),
                "contentBlocks":native.content.len(),"isError":is_error,"terminate":native.terminate}});
        state.results.0.insert(
            id.to_owned(),
            Retained {
                native,
                is_error,
                encoded: encoded.into_boxed_str(),
                _bytes: bytes,
            },
        );
        Ok(manifest)
    }
    pub(super) fn read_result(&self, args: &Value) -> Result<Value, String> {
        let id = args["resultId"].as_str().ok_or("resultId is required")?;
        let offset = integer(args, "offset", 0, 0, u64::MAX)?;
        let limit = integer(args, "limit", 65536, 4, 65536)? as usize;
        let state = self.state.lock().unwrap();
        if self.signal.is_cancelled() {
            return Err("parent invocation is no longer active".into());
        }
        let retained = state
            .results
            .0
            .get(id)
            .ok_or("unknown result reference for this invocation")?;
        let text = &retained.encoded;
        let offset = usize::try_from(offset).map_err(|_| "offset is past EOF")?;
        if offset > text.len() || !text.is_char_boundary(offset) {
            return Err("offset must be a UTF-8 boundary at or before EOF".into());
        }
        let mut end = offset.saturating_add(limit).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let reply = json!({"resultId":id,"offset":offset,"data":&text[offset..end],
            "byteLength":text.len(),"nextOffset":if end < text.len() {Some(end)} else {None}});
        // Even worst-case JSON escaping of 64 KiB stays below the frame limit.
        bounded(&reply)?;
        Ok(reply)
    }
    pub(super) fn release_result(&self, args: &Value) -> Result<Value, String> {
        let id = args["resultId"].as_str().ok_or("resultId is required")?;
        self.state
            .lock()
            .unwrap()
            .results
            .0
            .remove(id)
            .ok_or("unknown result reference for this invocation")?;
        Ok(json!({"resultId":id,"status":"released"}))
    }
    /// Resolve native authority only within this owning program's active lifetime.
    /// Usage/names remain in the receipt for inspection but are merged by host
    /// accounting, never by the attachment. No transcript calls are synthesized.
    pub fn resolve_result_reference(
        &self,
        id: &str,
    ) -> Result<(crate::AgentToolResult, bool), String> {
        let state = self.state.lock().unwrap();
        if self.signal.is_cancelled() {
            return Err("parent invocation is no longer active".into());
        }
        let retained = state
            .results
            .0
            .get(id)
            .ok_or("unknown result reference for this invocation")?;
        let mut result = retained.native.clone();
        result.usage = None;
        result.added_tool_names.clear();
        Ok((result, retained.is_error))
    }
}

const CONTENT_BLOCKS: usize = 256;
const CONTENT_BYTES: usize = 32 * 1024 * 1024;

/// Fallible serialized-byte sink: no payload JSON/string allocation.
#[derive(Default)]
struct ContentBudget {
    blocks: usize,
    encoded_bytes: usize,
}
impl ContentBudget {
    fn block(&mut self, block: &TextContentOrImage) -> Result<(), String> {
        if self.blocks == CONTENT_BLOCKS {
            return Err("content exceeds 256 blocks".into());
        }
        // n blocks require n-1 commas and two brackets.
        let mut counter = ByteCounter {
            bytes: 0,
            remaining: CONTENT_BYTES
                .checked_sub(self.encoded_bytes + self.blocks + 2)
                .ok_or("content array exceeds 32 MiB")?,
        };
        match block {
            TextContentOrImage::Text(text) => serde_json::to_writer(&mut counter, text),
            TextContentOrImage::Image(image) => serde_json::to_writer(&mut counter, image),
        }
        .map_err(|e| e.to_string())?;
        self.encoded_bytes += counter.bytes;
        self.blocks += 1;
        Ok(())
    }
}
struct ByteCounter {
    bytes: usize,
    remaining: usize,
}
impl std::io::Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(std::io::Error::other("content array exceeds 32 MiB"));
        }
        self.bytes += bytes.len();
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn visit_selected(
    blocks: &[TextContentOrImage],
    indices: Option<&[usize]>,
    mut visit: impl FnMut(&TextContentOrImage) -> Result<(), String>,
) -> Result<(), String> {
    let count = indices.map_or(blocks.len(), <[usize]>::len);
    if count > CONTENT_BLOCKS {
        return Err("content selection exceeds 256 blocks".into());
    }
    for position in 0..count {
        let index = indices.map_or(position, |indices| indices[position]);
        visit(
            blocks
                .get(index)
                .ok_or("content index is past native content")?,
        )?;
    }
    Ok(())
}
impl ToolProgram {
    fn check_content_live(&self) -> Result<(), String> {
        if self.signal.is_cancelled() {
            Err("parent invocation is no longer active".into())
        } else {
            Ok(())
        }
    }
    pub(super) fn measure_result_content(&self, args: &Value) -> Result<Value, String> {
        self.check_content_live()?;
        let id = args["resultId"].as_str().ok_or("resultId is required")?;
        let indices = match args.get("indices") {
            None | Some(Value::Null) => None,
            Some(Value::Array(values)) if values.len() <= CONTENT_BLOCKS => Some(
                values
                    .iter()
                    .map(|v| {
                        v.as_u64()
                            .and_then(|n| usize::try_from(n).ok())
                            .ok_or_else(|| "indices must contain nonnegative integers".to_owned())
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            _ => return Err("indices must be null or an array of at most 256 integers".into()),
        };
        let state = self.state.lock().unwrap();
        let retained = state
            .results
            .0
            .get(id)
            .ok_or("unknown result reference for this invocation")?;
        let mut budget = ContentBudget::default();
        visit_selected(&retained.native.content, indices.as_deref(), |block| {
            self.check_content_live()?;
            budget.block(block)
        })?;
        self.check_content_live()?;
        let reply = json!({"resultId":id,"contentBlocks":budget.blocks,"encodedBytes":budget.encoded_bytes});
        bounded(&reply)?;
        Ok(reply)
    }
    /// Resolve from this invocation's authentic native store. Count each block
    /// before cloning it, including repeated selections, to bound expansion.
    /// The serialized array limit is not an RSS ceiling or a retained-tree charge.
    pub fn resolve_content_parts(
        &self,
        parts: Vec<ToolProgramContentPart>,
    ) -> Result<Vec<TextContentOrImage>, String> {
        self.check_content_live()?;
        if parts.len() > CONTENT_BLOCKS {
            return Err("content exceeds 256 parts".into());
        }
        let state = self.state.lock().unwrap();
        let mut budget = ContentBudget::default();
        let mut content = Vec::new();
        for part in parts {
            self.check_content_live()?;
            match part {
                ToolProgramContentPart::Inline(blocks) => {
                    for block in blocks {
                        self.check_content_live()?;
                        budget.block(&block)?;
                        content.push(block);
                    }
                }
                ToolProgramContentPart::Reference { id, indices } => {
                    let retained = state
                        .results
                        .0
                        .get(&id)
                        .ok_or("unknown result reference for this invocation")?;
                    visit_selected(&retained.native.content, indices.as_deref(), |block| {
                        self.check_content_live()?;
                        budget.block(block)?;
                        content.push(block.clone());
                        Ok(())
                    })?;
                }
            }
        }
        self.check_content_live()?;
        Ok(content)
    }
}
