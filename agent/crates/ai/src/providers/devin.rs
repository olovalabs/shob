//! Native Codeium Cascade/Devin Connect transport (`devin-agent`).
//! Wire fields follow OMP's MIT-licensed devin.ts and Codeium protobuf schemas.
use super::cursor::wire::{self, data, fields, number, text, Output, Proto, Result};
use crate::event_stream::{
    create_assistant_message_event_stream, AssistantMessageEventStreamProducer,
};
use crate::types::{AssistantMessageEvent as Event, Content, ErrorReason, StopReason, UserContent};
use crate::{AssistantMessageEventStream, Context, Message, Model, Provider, SimpleStreamOptions};
use futures::StreamExt;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Read;

pub const API: &str = "devin-agent";
pub const DEFAULT_BASE_URL: &str = "https://server.codeium.com";
const AUTH_PATH: &str = "/exa.auth_pb.AuthService/GetUserJwt";
const ASSIGN_PATH: &str = "/exa.api_server_pb.ApiServerService/AssignModel";
const CHAT_PATH: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";

#[cfg(test)]
#[path = "devin/tests.rs"]
mod native_tests;

pub struct DevinProvider {
    id: String,
    api_key: Option<String>,
    http: reqwest::Client,
    models: Vec<Model>,
}
impl DevinProvider {
    pub fn with_models_without_env_api_key(
        id: impl Into<String>,
        api_key: Option<String>,
        http: reqwest::Client,
        models: Vec<Model>,
    ) -> Self {
        Self {
            id: id.into(),
            api_key,
            http,
            models,
        }
    }
}
#[async_trait::async_trait]
impl Provider for DevinProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn models(&self) -> &[Model] {
        &self.models
    }
    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let (mut tx, stream) = create_assistant_message_event_stream();
        let (model, context, mut options, http) = (
            model.clone(),
            context.clone(),
            options.clone(),
            self.http.clone(),
        );
        if options.api_key.is_none() {
            options.api_key = self.api_key.clone();
        }
        tokio::spawn(async move {
            let mut output = Output::new(&model);
            let cancellation = tx.cancellation_token();
            let timeout = options
                .timeout
                .unwrap_or(crate::provider::DEFAULT_LLM_API_TIMEOUT);
            let auth = wire::unwrap_api_key(&mut options.api_key);
            let result = tokio::select! {
                biased;
                _=options.signal.cancelled()=>Err("Request aborted".into()),
                _=cancellation.cancelled()=>Err("Request aborted".into()),
                result=tokio::time::timeout(timeout,async { auth?; run(&http,&model,&context,&options,&mut output,&mut tx).await })=>result.unwrap_or_else(|_|Err("Devin request timed out".into())),
            };
            if let Err(error) = result {
                let aborted = options.signal.is_cancelled() || cancellation.is_cancelled();
                output.message.stop_reason = if aborted {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                output.message.error_message = Some(error);
                tx.push(Event::Error {
                    reason: if aborted {
                        ErrorReason::Aborted
                    } else {
                        ErrorReason::Error
                    },
                    error: output.message,
                })
                .await;
            }
        });
        stream
    }
}

fn setting<'a>(model: &'a Model, name: &str) -> Option<&'a Value> {
    let params = model.sampling_params.as_ref()?;
    params
        .get(name)
        .or_else(|| params.get("compatConfig")?.get(name))
}
fn metadata(key: Option<&str>, jwt: &str) -> Proto {
    let key = key.unwrap_or("").trim();
    let encoded = if !key.starts_with("devin-session-token$") && key.split('.').count() == 3 {
        format!("devin-session-token${key}")
    } else {
        key.to_owned()
    };
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };
    Proto::default()
        .bytes(1, "devin-cli")
        .bytes(28, "chisel")
        .bytes(7, "3000.12.43")
        .bytes(12, "chisel")
        .bytes(2, "3000.12.43")
        .bytes(3, encoded)
        .bytes(21, jwt)
        .bytes(4, "en")
        .bytes(5, os)
}
fn normalize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect()
}
fn tool_id(id: &str, api: &crate::Api) -> String {
    let id = if matches!(
        api,
        crate::Api::OpenaiResponses
            | crate::Api::OpenaiCodexResponses
            | crate::Api::AzureOpenaiResponses
    ) {
        id.split('|').next().unwrap_or(id)
    } else {
        id
    };
    normalize_id(id)
}
fn images(mut prompt: Proto, blocks: &[Content]) -> Proto {
    for block in blocks {
        if let Content::Image(image) = block {
            prompt = prompt.msg(
                10,
                Proto::default()
                    .bytes(1, image.data.as_bytes())
                    .bytes(2, &image.mime_type),
            );
        }
    }
    prompt
}
fn prompt(
    message: &Message,
    model: &Model,
    ids: &std::collections::HashMap<String, String>,
    id: &str,
) -> Proto {
    let prompt = Proto::default().bytes(1, id);
    match message {
        Message::User(user) => match &user.content {
            UserContent::Text(t) => prompt.int(2, 1).bytes(3, t),
            UserContent::Blocks(blocks) => images(
                prompt.int(2, 1).bytes(3, Content::text_only(blocks, "")),
                blocks,
            ),
        },
        Message::Assistant(assistant) => {
            let mut prompt = prompt
                .int(2, 2)
                .bytes(3, Content::text_only(&assistant.content, ""));
            let native = assistant.api == model.api
                && assistant.provider == model.provider
                && assistant.model == model.id;
            let mut thinking_text = String::new();
            let mut signature = None;
            for block in &assistant.content {
                match block {
                    Content::Thinking(thinking) => {
                        thinking_text.push_str(&thinking.thinking);
                        if native && signature.is_none() {
                            signature = thinking.thinking_signature.as_deref();
                        }
                    }
                    Content::ToolCall(tool) => {
                        prompt = prompt.msg(
                            6,
                            Proto::default()
                                .bytes(
                                    1,
                                    ids.get(&tool.id).map_or(tool.id.as_str(), String::as_str),
                                )
                                .bytes(2, &tool.name)
                                .bytes(3, tool.arguments.to_string()),
                        )
                    }
                    _ => {}
                }
            }
            if !thinking_text.is_empty() {
                prompt = prompt.bytes(11, thinking_text);
            }
            if let Some(signature) = signature {
                prompt = prompt.bytes(12, signature);
            }
            if native {
                if let Some(response_id) = &assistant.response_id {
                    prompt = prompt.bytes(1, response_id);
                }
            }
            prompt
        }
        Message::ToolResult(result) => images(
            prompt
                .int(2, 4)
                .bytes(3, Content::text_only(&result.content, ""))
                .bytes(
                    7,
                    ids.get(&result.tool_call_id)
                        .cloned()
                        .unwrap_or_else(|| normalize_id(&result.tool_call_id)),
                )
                .int(9, result.is_error as u64),
            &result.content,
        ),
    }
}
fn normalize_google_schema(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Array(types)) = map.get("type") {
                let nonnull: Vec<_> = types
                    .iter()
                    .filter(|v| v.as_str() != Some("null"))
                    .cloned()
                    .collect();
                let nullable = nonnull.len() != types.len();
                if nonnull.len() == 1 {
                    map.insert("type".into(), nonnull[0].clone());
                }
                if nullable {
                    map.insert("nullable".into(), Value::Bool(true));
                }
            }
            for child in map.values_mut() {
                normalize_google_schema(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                normalize_google_schema(child);
            }
        }
        _ => {}
    }
}
fn request(
    model: &Model,
    context: &Context,
    options: &SimpleStreamOptions,
    jwt: &str,
    cascade: &str,
    model_uid: &str,
    assignment: Option<&str>,
) -> Result<Proto> {
    let mut ids = std::collections::HashMap::new();
    for message in &context.messages {
        if let Message::Assistant(assistant) = message {
            for block in &assistant.content {
                if let Content::ToolCall(call) = block {
                    ids.insert(call.id.clone(), tool_id(&call.id, &assistant.api));
                }
            }
        }
    }
    let mut request = Proto::default()
        .msg(1, metadata(options.api_key.as_deref(), jwt))
        .bytes(2, context.system_prompt.as_deref().unwrap_or(""));
    for (index, message) in context.messages.iter().enumerate() {
        let role = match message {
            Message::User(_) => "user",
            Message::Assistant(_) => "assistant",
            Message::ToolResult(_) => "tool",
        };
        let hash = Sha256::digest(format!("{cascade}\0{index}\0{role}").as_bytes());
        let mut uuid: [u8; 16] = hash[..16].try_into().unwrap();
        uuid[6] = (uuid[6] & 15) | 0x40;
        uuid[8] = (uuid[8] & 63) | 0x80;
        request = request.msg(
            3,
            prompt(
                message,
                model,
                &ids,
                &uuid::Uuid::from_bytes(uuid).to_string(),
            ),
        );
    }
    let temperature = options.temperature.unwrap_or(1.0).max(
        setting(model, "temperatureFloor")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
    );
    let config = Proto::default()
        .int(1, 1)
        .int(
            2,
            options.max_tokens.unwrap_or(if model.max_tokens > 0 {
                model.max_tokens
            } else {
                64000
            }),
        )
        .int(3, 400)
        .double(5, temperature)
        .int(7, 40)
        .double(8, 0.95_f32 as f64);
    request = request
        .bytes(21, model_uid)
        .int(7, 5)
        .int(20, 1)
        .msg(8, config)
        .int(
            11,
            !setting(model, "supportsParallelToolCalls")
                .and_then(Value::as_bool)
                .unwrap_or(false) as u64,
        )
        .msg(12, Proto::default().bytes(1, "auto"))
        .msg(13, Proto::default().int(1, 1))
        .bytes(16, cascade)
        .bytes(22, uuid::Uuid::new_v4().to_string());
    if let Some(assignment) = assignment {
        request = request.bytes(26, assignment);
    }
    let google = model_uid.to_lowercase().contains("gemini")
        || model_uid.starts_with("MODEL_GOOGLE_GEMINI_");
    for tool in &context.tools {
        let mut schema = serde_json::to_value(&tool.parameters).map_err(|e| e.to_string())?;
        if google {
            normalize_google_schema(&mut schema);
        }
        request = request.msg(
            10,
            Proto::default()
                .bytes(1, &tool.name)
                .bytes(2, &tool.description)
                .bytes(3, schema.to_string()),
        );
    }
    Ok(request)
}
async fn unary(
    http: &reqwest::Client,
    url: String,
    headers: &reqwest::header::HeaderMap,
    body: Proto,
) -> Result<Vec<u8>> {
    let response = wire::checked(
        http.post(url)
            .headers(headers.clone())
            .header("content-type", "application/proto")
            .header("connect-protocol-version", "1")
            .header("accept", "*/*")
            .body(body.0)
            .send()
            .await
            .map_err(|e| e.to_string())?,
    )
    .await?;
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes.as_ref())
            .take((wire::MAX_FRAME + 1) as u64)
            .read_to_end(&mut out)
            .map_err(|e| e.to_string())?;
        if out.len() > wire::MAX_FRAME {
            return Err("Devin unary body exceeds limit".into());
        }
        Ok(out)
    } else {
        Ok(bytes.to_vec())
    }
}
async fn run(
    http: &reqwest::Client,
    model: &Model,
    context: &Context,
    options: &SimpleStreamOptions,
    output: &mut Output,
    tx: &mut AssistantMessageEventStreamProducer,
) -> Result<()> {
    let base = if model.base_url.is_empty() {
        DEFAULT_BASE_URL
    } else {
        &model.base_url
    }
    .trim_end_matches('/');
    let headers = wire::headers(model, options)?;
    let auth = unary(
        http,
        format!("{base}{AUTH_PATH}"),
        &headers,
        Proto::default().msg(1, metadata(options.api_key.as_deref(), "")),
    )
    .await?;
    let auth = fields(&auth)?;
    let jwt = text(&auth, 1)?;
    if jwt.is_empty() {
        return Err("Devin GetUserJwt returned an empty user JWT".into());
    }
    let custom = text(&auth, 2)?;
    let base = if custom.trim().is_empty() {
        base
    } else {
        custom.trim().trim_end_matches('/')
    };
    let cascade = options
        .session_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let uid = wire::request_model_id(model, options);
    let mut assigned_uid = None;
    let mut assignment_jwt = None;
    if setting(model, "modelRouter")
        .and_then(Value::as_bool)
        .unwrap_or(uid == "adaptive")
    {
        let mut body = Proto::default()
            .msg(1, metadata(options.api_key.as_deref(), ""))
            .bytes(2, uid)
            .bytes(3, &cascade);
        if let Some(message) = context
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m, Message::User(_)))
        {
            body = body.msg(5, prompt(message, model, &Default::default(), ""));
        }
        let assignment = unary(http, format!("{base}{ASSIGN_PATH}"), &headers, body).await?;
        let root = fields(&assignment)?;
        let assignment = fields(data(&root, 1))?;
        let jwt = text(&assignment, 1)?;
        let uid = text(&assignment, 2)?;
        if jwt.is_empty() || uid.is_empty() {
            return Err("Devin AssignModel returned no assignment JWT/model UID".into());
        }
        assigned_uid = Some(uid);
        assignment_jwt = Some(jwt);
    }
    let body = request(
        model,
        context,
        options,
        &jwt,
        &cascade,
        assigned_uid.as_deref().unwrap_or(uid),
        assignment_jwt.as_deref(),
    )?;
    let response = wire::checked(
        http.post(format!("{base}{CHAT_PATH}"))
            .headers(headers)
            .header("content-type", "application/connect+proto")
            .header("connect-protocol-version", "1")
            .header("accept-encoding", "identity")
            .header("connect-accept-encoding", "gzip")
            .header("user-agent", "connect-go/1.18.1 (go1.26.3)")
            .body(wire::frame(&body.0))
            .send()
            .await
            .map_err(|e| e.to_string())?,
    )
    .await?;
    output.message.response_model = assigned_uid;
    output.start(tx).await;
    let mut stream = response.bytes_stream();
    let mut decoder = wire::Decoder::default();
    let mut ended = false;
    let mut length = false;
    loop {
        while let Some((flag, bytes)) = decoder.next()? {
            if ended {
                return Err("Devin data after Connect trailer".into());
            }
            if flag & 2 != 0 {
                wire::trailer(&bytes)?;
                ended = true;
                continue;
            }
            let f = fields(&bytes)?;
            let id = text(&f, 1)?;
            if !id.is_empty() {
                output.message.response_id = Some(id);
            }
            let model = text(&f, 23)?;
            if !model.is_empty() {
                output.message.response_model = Some(model);
            }
            let signature = text(&f, 10)?;
            output
                .delta(
                    tx,
                    text(&f, 9)?,
                    true,
                    (!signature.is_empty()).then_some(signature),
                )
                .await;
            output.delta(tx, text(&f, 3)?, false, None).await;
            for tool in f.iter().filter(|f| f.n == 6) {
                let tool = fields(tool.value)?;
                output
                    .tool(tx, text(&tool, 1)?, text(&tool, 2)?, text(&tool, 3)?)
                    .await?;
            }
            match number(&f, 5) {
                3 => length = true,
                13 => return Err("Devin model reported STOP_REASON_ERROR".into()),
                _ => {}
            }
            if !data(&f, 7).is_empty() {
                let usage = fields(data(&f, 7))?;
                output.message.usage.input = number(&usage, 2) as i64;
                output.message.usage.output = number(&usage, 3) as i64;
                output.message.usage.cache_write = number(&usage, 4) as i64;
                output.message.usage.cache_read = number(&usage, 5) as i64;
            }
        }
        if ended {
            break;
        }
        let idle = options
            .stream_idle_timeout
            .unwrap_or(crate::provider::DEFAULT_STREAM_IDLE_TIMEOUT);
        let chunk = if idle.is_zero() {
            stream.next().await
        } else {
            tokio::time::timeout(idle, stream.next())
                .await
                .map_err(|_| "Devin stream idle timeout")?
        };
        match chunk {
            Some(Ok(bytes)) => decoder.push(&bytes),
            Some(Err(e)) => return Err(e.to_string()),
            None => break,
        }
    }
    decoder.finish()?;
    if !ended {
        return Err("Devin stream ended without a Connect trailer".into());
    }
    output.finish(tx, length).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn model() -> Model {
        Model::new(
            "test",
            "Test",
            crate::Api::Other(API.into()),
            "devin",
            DEFAULT_BASE_URL,
        )
    }
    #[test]
    fn credentials_and_client_identity_match_native_cli() {
        let bytes = metadata(Some("header.payload.signature"), "jwt").0;
        let f = fields(&bytes).unwrap();
        assert_eq!(
            text(&f, 3).unwrap(),
            "devin-session-token$header.payload.signature"
        );
        assert_eq!(text(&f, 28).unwrap(), "chisel");
        assert_eq!(text(&f, 21).unwrap(), "jwt");
        let bytes = metadata(Some("sk-ws-key"), "").0;
        assert_eq!(text(&fields(&bytes).unwrap(), 3).unwrap(), "sk-ws-key");
    }
    #[test]
    fn request_is_cascade_protobuf_not_openai_json() {
        let bytes = request(
            &model(),
            &Context::default(),
            &SimpleStreamOptions::default(),
            "jwt",
            "cascade",
            "uid",
            Some("assignment"),
        )
        .unwrap()
        .0;
        let f = fields(&bytes).unwrap();
        assert_eq!(number(&f, 7), 5);
        assert_eq!(text(&f, 21).unwrap(), "uid");
        assert_eq!(text(&f, 26).unwrap(), "assignment");
        assert_eq!(text(&f, 16).unwrap(), "cascade");
        let cfg = fields(data(&f, 8)).unwrap();
        assert_eq!(number(&cfg, 1), 1);
        assert_eq!(number(&cfg, 2), 64000);
    }
    #[tokio::test]
    async fn tool_arguments_support_cumulative_and_fragment_deltas() {
        let (mut tx, stream) = create_assistant_message_event_stream();
        let mut out = Output::new(&model());
        out.tool(&mut tx, "call".into(), "read".into(), "{\"path\":".into())
            .await
            .unwrap();
        out.tool(
            &mut tx,
            String::new(),
            String::new(),
            "{\"path\":\"file\"".into(),
        )
        .await
        .unwrap();
        out.tool(&mut tx, String::new(), String::new(), "}".into())
            .await
            .unwrap();
        out.finish(&mut tx, false).await.unwrap();
        let result = stream.result().await.unwrap();
        let Content::ToolCall(call) = &result.content[0] else {
            panic!()
        };
        assert_eq!(call.arguments, json!({"path":"file"}));
        assert_eq!(result.stop_reason, StopReason::ToolUse);
    }
}
