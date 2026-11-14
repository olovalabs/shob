//! Minimal protobuf and Connect codecs for the native agent protocols.
use crate::event_stream::AssistantMessageEventStreamProducer;
use crate::types::{Content, DoneReason, TextContent, ThinkingContent, ToolCall};
use crate::{AssistantMessage, AssistantMessageEvent as Event, Model};
use bytes::{Buf, Bytes, BytesMut};
use serde_json::{json, Value};
use std::{collections::HashMap, io::Read, sync::Arc};

pub(in crate::providers) const MAX_FRAME: usize = 16 * 1024 * 1024;
pub(in crate::providers) type Result<T, E = String> = std::result::Result<T, E>;

pub(in crate::providers) fn unwrap_api_key(key: &mut Option<String>) -> Result<()> {
    let Some(encoded) = key
        .as_deref()
        .filter(|key| key.trim_start().starts_with('{'))
    else {
        return Ok(());
    };
    let value: Value = serde_json::from_str(encoded)
        .map_err(|e| format!("Invalid native provider credential: {e}"))?;
    let Value::Object(mut credential) = value else {
        return Err("Native provider credential must be an object".into());
    };
    let Some(Value::String(token)) = credential.remove("token") else {
        return Err("Native provider credential has no access token".into());
    };
    *key = Some(token);
    Ok(())
}

pub(in crate::providers) fn request_model_id<'a>(
    model: &'a Model,
    options: &'a crate::SimpleStreamOptions,
) -> &'a str {
    if let Some(id) = options
        .metadata
        .as_ref()
        .and_then(|m| m.get("request_model_id"))
    {
        return id;
    }
    let params = model.sampling_params.as_ref();
    let effort = match options.reasoning.unwrap_or_default() {
        crate::types::ThinkingLevel::Off => "off",
        crate::types::ThinkingLevel::Minimal => "minimal",
        crate::types::ThinkingLevel::Low => "low",
        crate::types::ThinkingLevel::Medium => "medium",
        crate::types::ThinkingLevel::High => "high",
        crate::types::ThinkingLevel::Xhigh => "xhigh",
        crate::types::ThinkingLevel::Max => "max",
    };
    params
        .and_then(|p| p.get("thinking"))
        .and_then(|p| p.get("effortRouting"))
        .and_then(|p| p.get(effort))
        .and_then(Value::as_str)
        .or_else(|| {
            params
                .and_then(|p| p.get("requestModelId"))
                .and_then(Value::as_str)
        })
        .unwrap_or(&model.id)
}

#[derive(Default)]
pub(in crate::providers) struct Proto(pub Vec<u8>);
impl Proto {
    pub fn bytes(mut self, n: u32, value: impl AsRef<[u8]>) -> Self {
        let value = value.as_ref();
        varint(&mut self.0, ((n as u64) << 3) | 2);
        varint(&mut self.0, value.len() as u64);
        self.0.extend_from_slice(value);
        self
    }
    pub fn int(mut self, n: u32, value: u64) -> Self {
        varint(&mut self.0, (n as u64) << 3);
        varint(&mut self.0, value);
        self
    }
    pub fn double(mut self, n: u32, value: f64) -> Self {
        varint(&mut self.0, ((n as u64) << 3) | 1);
        self.0.extend_from_slice(&value.to_le_bytes());
        self
    }
    pub fn msg(self, n: u32, value: Proto) -> Self {
        self.bytes(n, value.0)
    }
}
fn varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 128 {
        out.push((n as u8) | 128);
        n >>= 7;
    }
    out.push(n as u8);
}
fn take_varint(data: &mut &[u8]) -> Result<u64> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let Some((&b, rest)) = data.split_first() else {
            return Err("Truncated protobuf varint".into());
        };
        *data = rest;
        if shift == 63 && b > 1 {
            return Err("Protobuf varint overflow".into());
        }
        value |= ((b & 127) as u64) << shift;
        if b < 128 {
            return Ok(value);
        }
    }
    Err("Protobuf varint overflow".into())
}
#[derive(Clone, Copy)]
pub(in crate::providers) struct Field<'a> {
    pub n: u32,
    pub value: &'a [u8],
    pub integer: u64,
}
pub(in crate::providers) fn fields(mut data: &[u8]) -> Result<Vec<Field<'_>>> {
    let mut out = Vec::new();
    while !data.is_empty() {
        let tag = take_varint(&mut data)?;
        let n = u32::try_from(tag >> 3).map_err(|_| "Invalid protobuf tag")?;
        if n == 0 {
            return Err("Invalid protobuf field zero".into());
        }
        let (value, integer) = match tag & 7 {
            0 => (&[][..], take_varint(&mut data)?),
            wire @ (1 | 2 | 5) => {
                let size = match wire {
                    1 => 8,
                    5 => 4,
                    _ => usize::try_from(take_varint(&mut data)?)
                        .map_err(|_| "Invalid protobuf length")?,
                };
                if size > data.len() {
                    return Err("Truncated protobuf field".into());
                }
                let (value, rest) = data.split_at(size);
                data = rest;
                (value, 0)
            }
            _ => return Err("Unsupported protobuf wire type".into()),
        };
        out.push(Field { n, value, integer });
    }
    Ok(out)
}
pub(in crate::providers) fn data<'a>(f: &[Field<'a>], n: u32) -> &'a [u8] {
    f.iter().rev().find(|f| f.n == n).map_or(&[], |f| f.value)
}
pub(in crate::providers) fn number(f: &[Field<'_>], n: u32) -> u64 {
    f.iter().rev().find(|f| f.n == n).map_or(0, |f| f.integer)
}
pub(in crate::providers) fn text(f: &[Field<'_>], n: u32) -> Result<String> {
    String::from_utf8(data(f, n).to_vec()).map_err(|_| "Invalid protobuf UTF-8".into())
}

pub(in crate::providers) fn frame(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 5);
    out.push(0);
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
    out
}
#[derive(Default)]
pub(in crate::providers) struct Decoder {
    buffer: BytesMut,
}
impl Decoder {
    pub fn push(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }
    pub fn next(&mut self) -> Result<Option<(u8, Bytes)>> {
        if self.buffer.len() < 5 {
            return Ok(None);
        }
        let n = u32::from_be_bytes(self.buffer[1..5].try_into().unwrap()) as usize;
        if n > MAX_FRAME {
            return Err(format!("Connect payload exceeds {MAX_FRAME} bytes"));
        }
        if self.buffer.len() < n + 5 {
            return Ok(None);
        }
        let flag = self.buffer[0];
        self.buffer.advance(5);
        let raw = self.buffer.split_to(n).freeze();
        if flag & !3 != 0 {
            return Err("Unknown Connect frame flags".into());
        }
        if flag & 1 == 0 {
            return Ok(Some((flag, raw)));
        }
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(raw.as_ref())
            .take((MAX_FRAME + 1) as u64)
            .read_to_end(&mut out)
            .map_err(|e| format!("Invalid Connect gzip: {e}"))?;
        if out.len() > MAX_FRAME {
            return Err("Inflated Connect payload exceeds limit".into());
        }
        Ok(Some((flag, Bytes::from(out))))
    }
    pub fn finish(&self) -> Result<()> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err("Truncated Connect frame".into())
        }
    }
}
pub(in crate::providers) fn trailer(data: &[u8]) -> Result<()> {
    let value: Value =
        serde_json::from_slice(data).map_err(|e| format!("Invalid Connect trailer: {e}"))?;
    if let Some(error) = value.get("error").filter(|e| !e.is_null()) {
        return Err(format!(
            "Connect {}: {}",
            error["code"].as_str().unwrap_or("error"),
            error["message"].as_str().unwrap_or("stream rejected")
        ));
    }
    Ok(())
}
pub(in crate::providers) fn json_encode(value: &Value) -> Proto {
    match value {
        Value::Null => Proto::default().int(1, 0),
        Value::Bool(b) => Proto::default().int(4, *b as u64),
        Value::Number(n) => Proto::default().double(2, n.as_f64().unwrap_or(0.0)),
        Value::String(s) => Proto::default().bytes(3, s),
        Value::Array(values) => Proto::default().msg(
            6,
            values
                .iter()
                .fold(Proto::default(), |p, v| p.msg(1, json_encode(v))),
        ),
        Value::Object(values) => Proto::default().msg(
            5,
            values.iter().fold(Proto::default(), |p, (k, v)| {
                p.msg(1, Proto::default().bytes(1, k).msg(2, json_encode(v)))
            }),
        ),
    }
}
pub(in crate::providers) fn json_decode(data_bytes: &[u8]) -> Result<Value> {
    json_decode_depth(data_bytes, 0)
}
fn json_decode_depth(bytes: &[u8], depth: usize) -> Result<Value> {
    if depth > 64 {
        return Err("Protobuf JSON nesting exceeds limit".into());
    }
    let f = fields(bytes)?;
    let Some(v) = f.last() else {
        return Ok(Value::Null);
    };
    Ok(match v.n {
        1 => Value::Null,
        2 if v.value.len() == 8 => json!(f64::from_le_bytes(v.value.try_into().unwrap())),
        3 => Value::String(text(&f, 3)?),
        4 => Value::Bool(v.integer != 0),
        5 => {
            let mut map = serde_json::Map::new();
            for entry in fields(v.value)?.into_iter().filter(|f| f.n == 1) {
                let e = fields(entry.value)?;
                map.insert(text(&e, 1)?, json_decode_depth(data(&e, 2), depth + 1)?);
            }
            Value::Object(map)
        }
        6 => Value::Array(
            fields(v.value)?
                .into_iter()
                .filter(|f| f.n == 1)
                .map(|f| json_decode_depth(f.value, depth + 1))
                .collect::<Result<_>>()?,
        ),
        _ => return Err("Invalid protobuf JSON value".into()),
    })
}

pub(in crate::providers) struct Output {
    pub message: AssistantMessage,
    cost: crate::types::ModelCost,
    text: Option<usize>,
    thinking: Option<usize>,
    tools: HashMap<String, (usize, String)>,
    active_tool: Option<String>,
}
impl Output {
    pub fn new(model: &Model) -> Self {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        Self {
            message: AssistantMessage::empty(
                model.api.clone(),
                model.provider.clone(),
                model.id.clone(),
                timestamp,
            ),
            cost: model.cost.clone(),
            text: None,
            thinking: None,
            tools: HashMap::new(),
            active_tool: None,
        }
    }
    pub fn snapshot(&self) -> Arc<AssistantMessage> {
        Arc::new(self.message.clone())
    }
    pub async fn start(&self, tx: &mut AssistantMessageEventStreamProducer) {
        tx.push(Event::Start {
            partial: self.snapshot(),
        })
        .await;
    }
    pub async fn end_blocks(&mut self, tx: &mut AssistantMessageEventStreamProducer) {
        if let Some(i) = self.text.take() {
            if let Content::Text(t) = &self.message.content[i] {
                tx.push(Event::TextEnd {
                    content_index: i,
                    content: t.text.clone(),
                    partial: self.snapshot(),
                })
                .await;
            }
        }
        if let Some(i) = self.thinking.take() {
            if let Content::Thinking(t) = &self.message.content[i] {
                tx.push(Event::ThinkingEnd {
                    content_index: i,
                    content: t.thinking.clone(),
                    partial: self.snapshot(),
                })
                .await;
            }
        }
    }
    pub async fn delta(
        &mut self,
        tx: &mut AssistantMessageEventStreamProducer,
        value: String,
        thinking: bool,
        signature: Option<String>,
    ) {
        if value.is_empty() {
            if thinking {
                if let Some(signature) = signature {
                    if let Some(Content::Thinking(block)) = self
                        .message
                        .content
                        .iter_mut()
                        .rev()
                        .find(|b| matches!(b, Content::Thinking(_)))
                    {
                        block.thinking_signature = Some(signature);
                    }
                }
            }
            return;
        }
        if (thinking && self.text.is_some()) || (!thinking && self.thinking.is_some()) {
            self.end_blocks(tx).await;
        }
        let existing = if thinking { self.thinking } else { self.text };
        let i = match existing {
            Some(i) => i,
            None => {
                let i = self.message.content.len();
                self.message.content.push(if thinking {
                    Content::Thinking(ThinkingContent {
                        kind: Default::default(),
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: false,
                    })
                } else {
                    Content::Text(TextContent {
                        kind: Default::default(),
                        text: String::new(),
                        text_signature: None,
                    })
                });
                if thinking {
                    self.thinking = Some(i);
                    tx.push(Event::ThinkingStart {
                        content_index: i,
                        partial: self.snapshot(),
                    })
                    .await;
                } else {
                    self.text = Some(i);
                    tx.push(Event::TextStart {
                        content_index: i,
                        partial: self.snapshot(),
                    })
                    .await;
                }
                i
            }
        };
        match &mut self.message.content[i] {
            Content::Thinking(t) => {
                t.thinking.push_str(&value);
                if signature.is_some() {
                    t.thinking_signature = signature;
                }
            }
            Content::Text(t) => t.text.push_str(&value),
            _ => unreachable!(),
        }
        let event = if thinking {
            Event::ThinkingDelta {
                content_index: i,
                delta: value,
                partial: self.snapshot(),
            }
        } else {
            Event::TextDelta {
                content_index: i,
                delta: value,
                partial: self.snapshot(),
            }
        };
        tx.push(event).await;
    }
    pub async fn tool(
        &mut self,
        tx: &mut AssistantMessageEventStreamProducer,
        mut id: String,
        name: String,
        args: String,
    ) -> Result<()> {
        self.end_blocks(tx).await;
        if id.is_empty() {
            id = self
                .active_tool
                .clone()
                .ok_or("Tool argument delta has no call id")?;
        }
        if !self.tools.contains_key(&id) {
            let i = self.message.content.len();
            self.message.content.push(Content::ToolCall(ToolCall {
                kind: Default::default(),
                id: id.clone(),
                name: name.clone(),
                arguments: json!({}),
                thought_signature: None,
                namespace: None,
                recorded: None,
            }));
            self.tools.insert(id.clone(), (i, String::new()));
            tx.push(Event::ToolCallStart {
                content_index: i,
                partial: self.snapshot(),
            })
            .await;
        }
        self.active_tool = Some(id.clone());
        let (i, buffer) = self.tools.get_mut(&id).unwrap();
        let i = *i;
        let delta = if args.starts_with(buffer.as_str()) {
            let delta = args[buffer.len()..].to_owned();
            *buffer = args;
            delta
        } else {
            buffer.push_str(&args);
            args
        };
        if let Content::ToolCall(t) = &mut self.message.content[i] {
            if !name.is_empty() {
                t.name = name;
            }
            if let Ok(value) = serde_json::from_str(buffer) {
                t.arguments = value;
            }
        }
        if !delta.is_empty() {
            tx.push(Event::ToolCallDelta {
                content_index: i,
                delta,
                partial: self.snapshot(),
            })
            .await;
        }
        Ok(())
    }
    pub async fn tool_snapshot(
        &mut self,
        tx: &mut AssistantMessageEventStreamProducer,
        id: String,
        name: String,
        args: Value,
    ) -> Result<()> {
        let serialized = args.to_string();
        let Some((i, buffer)) = self.tools.get_mut(&id) else {
            return self.tool(tx, id, name, serialized).await;
        };
        let i = *i;
        let delta = serialized
            .strip_prefix(buffer.as_str())
            .unwrap_or("")
            .to_owned();
        *buffer = serialized;
        if let Content::ToolCall(call) = &mut self.message.content[i] {
            if !name.is_empty() {
                call.name = name;
            }
            call.arguments = args;
        }
        tx.push(Event::ToolCallDelta {
            content_index: i,
            delta,
            partial: self.snapshot(),
        })
        .await;
        Ok(())
    }
    pub fn has_tools(&self) -> bool {
        !self.tools.is_empty()
    }
    pub async fn finish(
        &mut self,
        tx: &mut AssistantMessageEventStreamProducer,
        length: bool,
    ) -> Result<()> {
        self.end_blocks(tx).await;
        for i in 0..self.message.content.len() {
            let Content::ToolCall(call) = &mut self.message.content[i] else {
                continue;
            };
            let args = &self
                .tools
                .get(&call.id)
                .expect("streamed tool has an argument buffer")
                .1;
            call.arguments = if args.is_empty() {
                json!({})
            } else {
                serde_json::from_str(args)
                    .map_err(|e| format!("Invalid arguments for tool {}: {e}", call.name))?
            };
            let tool_call = call.clone();
            tx.push(Event::ToolCallEnd {
                content_index: i,
                tool_call,
                partial: self.snapshot(),
            })
            .await;
        }
        let reason = if self.has_tools() {
            DoneReason::ToolUse
        } else if length {
            DoneReason::Length
        } else {
            DoneReason::Stop
        };
        self.message.stop_reason = reason.into();
        self.message.usage.total_tokens = self.message.usage.input
            + self.message.usage.output
            + self.message.usage.cache_read
            + self.message.usage.cache_write;
        self.message.usage.cost =
            crate::providers::anthropic::cost::calculate_cost(&self.cost, &self.message.usage);
        tx.push(Event::Done {
            reason,
            message: self.message.clone(),
        })
        .await;
        Ok(())
    }
}

pub(in crate::providers) fn headers(
    model: &Model,
    options: &crate::SimpleStreamOptions,
) -> Result<reqwest::header::HeaderMap> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in model
        .headers
        .iter()
        .chain(options.headers.iter())
        .flat_map(|m| m.iter())
    {
        headers.insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|e| e.to_string())?,
            value
                .parse::<reqwest::header::HeaderValue>()
                .map_err(|e| e.to_string())?,
        );
    }
    Ok(headers)
}
pub(in crate::providers) async fn checked(
    response: reqwest::Response,
) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let detail = value["message"]
        .as_str()
        .or_else(|| value["error"]["message"].as_str())
        .unwrap_or("");
    Err(format!(
        "HTTP {status}: {}",
        detail.chars().take(2000).collect::<String>()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credential_wrapper_uses_only_the_raw_access_token() {
        let mut key = Some(json!({"token":"raw-token","email":"account@example.test"}).to_string());
        unwrap_api_key(&mut key).unwrap();
        assert_eq!(key.as_deref(), Some("raw-token"));
        let mut key = Some("raw-key".into());
        unwrap_api_key(&mut key).unwrap();
        assert_eq!(key.as_deref(), Some("raw-key"));
    }
    #[test]
    fn compressed_connect_payload_is_decoded() {
        use std::io::Write;
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(b"protobuf payload").unwrap();
        let mut bytes = frame(&gzip.finish().unwrap());
        bytes[0] = 1;
        let mut decoder = Decoder::default();
        decoder.push(&bytes);
        assert_eq!(
            decoder.next().unwrap().unwrap().1.as_ref(),
            b"protobuf payload"
        );
        decoder.finish().unwrap();
    }
    #[tokio::test]
    async fn authoritative_tool_snapshot_replaces_partial_arguments() {
        let model = Model::new(
            "test",
            "Test",
            crate::Api::Other("cursor-agent".into()),
            "cursor",
            "",
        );
        let (mut tx, stream) = crate::event_stream::create_assistant_message_event_stream();
        let mut output = Output::new(&model);
        output
            .tool(&mut tx, "call".into(), "lookup".into(), "{\"x\":1}".into())
            .await
            .unwrap();
        output
            .tool_snapshot(
                &mut tx,
                "call".into(),
                "lookup".into(),
                json!({"x":1,"y":2}),
            )
            .await
            .unwrap();
        output.finish(&mut tx, false).await.unwrap();
        let result = stream.result().await.unwrap();
        assert!(
            matches!(&result.content[0],Content::ToolCall(call) if call.arguments==json!({"x":1,"y":2}))
        );
    }
    #[test]
    fn connect_decodes_fragmented_and_coalesced_frames() {
        let first = frame(b"one");
        let second = frame(b"two");
        let mut decoder = Decoder::default();
        for b in &first[..7] {
            decoder.push(&[*b]);
            assert!(decoder.next().unwrap().is_none());
        }
        decoder.push(&first[7..]);
        decoder.push(&second);
        assert_eq!(decoder.next().unwrap().unwrap().1.as_ref(), b"one");
        assert_eq!(decoder.next().unwrap().unwrap().1.as_ref(), b"two");
        decoder.finish().unwrap();
    }
    #[test]
    fn rejects_oversized_and_truncated_frames() {
        let mut decoder = Decoder::default();
        decoder.push(&[0, 1, 0, 0, 1]);
        assert!(decoder.next().is_err());
        let mut decoder = Decoder::default();
        decoder.push(&[0, 0]);
        assert!(decoder.finish().is_err());
    }
    #[test]
    fn json_protobuf_round_trip() {
        let value = json!({"x":[null,true,1.5,"hello"],"nested":{"z":false}});
        assert_eq!(json_decode(&json_encode(&value).0).unwrap(), value);
    }
    #[test]
    fn connect_trailer_errors_are_not_success() {
        assert!(
            trailer(br#"{"error":{"code":"unauthenticated","message":"expired"}}"#)
                .unwrap_err()
                .contains("expired")
        );
        assert!(trailer(b"{}").is_ok());
    }
}
