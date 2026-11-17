//! OpenAI-compatible Chat Completions provider.
//!
//! The `openai-completions` API name used by pi maps to the streaming
//! `/chat/completions` endpoint, including compatible third-party gateways.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use crate::event_stream::{
    create_assistant_message_event_stream_with_cancellation, AssistantMessageEventStream,
    AssistantMessageEventStreamProducer,
};
use crate::model::{MaxTokensField, Model, StreamingProtocolCompat};
use crate::provider::{Provider, SimpleStreamOptions};
use crate::providers::anthropic::cost::calculate_cost;
use crate::providers::anthropic::json_parse::parse_streaming_json;
use crate::providers::anthropic::retry::retry_provider_request;
use crate::providers::anthropic::sse::SseEventStream;
use crate::providers::{DELTA_FLUSH_BYTES, DELTA_FLUSH_INTERVAL};
use crate::types::{
    AssistantMessage, AssistantMessageEvent, Content, Context, DoneReason, ErrorReason,
    InputModality, Message, StopReason, ThinkingContent, ThinkingContentType, ThinkingLevel,
    ToolCall, ToolCallType, Usage, UserContent,
};
use crate::AiError;

/// Provider for OpenAI's streaming Chat Completions wire protocol.
pub struct OpenAiCompletionsProvider {
    id: String,
    api_key: Option<String>,
    allow_env_api_key: bool,
    require_auth: bool,
    http: reqwest::Client,
    models: Vec<Model>,
}

impl OpenAiCompletionsProvider {
    pub fn with_models(
        id: impl Into<String>,
        api_key: Option<String>,
        http: reqwest::Client,
        models: Vec<Model>,
    ) -> Self {
        let id = id.into();
        let allow_env_api_key = id == "openai";
        Self {
            id,
            api_key,
            allow_env_api_key,
            require_auth: true,
            http,
            models,
        }
    }

    /// Build a compatible custom provider without inheriting OpenAI's
    /// process-wide credential.
    pub fn with_models_without_env_api_key(
        id: impl Into<String>,
        api_key: Option<String>,
        http: reqwest::Client,
        models: Vec<Model>,
    ) -> Self {
        Self {
            id: id.into(),
            api_key,
            allow_env_api_key: false,
            require_auth: true,
            http,
            models,
        }
    }

    /// Explicit opt-in for local or proxy endpoints that accept anonymous
    /// requests. Never inherits the OpenAI environment credential.
    pub fn with_models_without_auth(
        id: impl Into<String>,
        http: reqwest::Client,
        models: Vec<Model>,
    ) -> Self {
        let mut provider = Self::with_models_without_env_api_key(id, None, http, models);
        provider.require_auth = false;
        provider
    }
}

struct PreparedRequest {
    headers: BTreeMap<String, String>,
    body: Value,
}

fn prepare_request<F>(
    model: &Model,
    ctx: &Context,
    opts: &SimpleStreamOptions,
    provider_key: &Option<String>,
    allow_env_api_key: bool,
    require_auth: bool,
    build_body: F,
) -> Result<PreparedRequest, String>
where
    F: FnOnce(&Model, &Context, &SimpleStreamOptions) -> Value,
{
    let api_key = resolve_api_key(provider_key, opts, allow_env_api_key);
    let mut headers = BTreeMap::new();
    for extra in [model.headers.as_ref(), opts.headers.as_ref()]
        .into_iter()
        .flatten()
    {
        for (name, value) in extra {
            crate::provider::set_http_header(&mut headers, name, Some(value));
        }
    }
    apply_api_key(&mut headers, api_key);
    // Identify rpi on provider requests (native `getPiUserAgent`), unless the
    // caller/model already set a User-Agent.
    crate::http::ensure_user_agent(&mut headers);
    if require_auth && !has_auth_header(&headers) {
        return Err(format!("No API key for provider: {}", model.provider));
    }

    // Do not build the wire body until the original auth/header checks pass.
    // The resulting Value is owned, so the spawned request need not retain the
    // caller's Context or any of its history.
    Ok(PreparedRequest {
        headers,
        body: build_body(model, ctx, opts),
    })
}

#[async_trait]
impl Provider for OpenAiCompletionsProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn models(&self) -> &[Model] {
        &self.models
    }

    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let (mut producer, stream) =
            create_assistant_message_event_stream_with_cancellation(&opts.signal);
        let http = self.http.clone();
        let provider_key = self.api_key.clone();
        let allow_env_api_key = self.allow_env_api_key;
        let require_auth = self.require_auth;
        // Prepare headers while the caller's Context is borrowed. This keeps
        // the original auth check ahead of the potentially expensive body
        // conversion; only an owned successful preparation crosses the spawn.
        let preparation = prepare_request(
            model,
            ctx,
            opts,
            &provider_key,
            allow_env_api_key,
            require_auth,
            build_request,
        );
        let model = model.clone();
        let mut opts = opts.clone();
        opts.signal = producer.cancellation_token();

        tokio::spawn(async move {
            run_stream(&mut producer, http, &model, preparation, &opts).await;
        });

        stream
    }
}

async fn run_stream(
    producer: &mut AssistantMessageEventStreamProducer,
    http: reqwest::Client,
    model: &Model,
    preparation: Result<PreparedRequest, String>,
    opts: &SimpleStreamOptions,
) {
    let mut state = StreamState::new(model);
    let PreparedRequest { headers, body } = match preparation {
        Ok(prepared) => prepared,
        Err(error) => {
            state.error(producer, error, false).await;
            return;
        }
    };

    let url = chat_completions_url(&model.base_url);
    // A silent/stalled gateway must not leave the harness in Working forever.
    // Callers can override this through SimpleStreamOptions when needed.
    let timeout = opts.request_timeout();
    let signal = opts.signal.clone();
    let mut request = http.post(&url).timeout(timeout);
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    let request = match super::request::PreparedJsonRequest::new(request, &body) {
        Ok(request) => request,
        Err(error) => {
            state
                .error(
                    producer,
                    format!("failed to encode request: {error}"),
                    false,
                )
                .await;
            return;
        }
    };
    drop(body);
    let response = retry_provider_request(
        move || {
            let request = request.clone();
            let signal = signal.clone();
            async move {
                let response = tokio::select! {
                    result = request.send() => result?,
                    _ = signal.cancelled() => return Err(AiError::Abort {
                        message: "Request aborted".into(),
                    }),
                };
                let status = response.status();
                if status.is_success() {
                    Ok(response)
                } else {
                    let code = status.as_u16();
                    let message = response.text().await.unwrap_or_default();
                    Err(AiError::Http {
                        status: Some(code),
                        message,
                    })
                }
            }
        },
        opts.max_retries,
        opts.max_retry_delay,
        &opts.signal,
    )
    .await;

    let response = match response {
        Ok(response) => response,
        Err(error) => {
            state
                .error(producer, error.to_string(), error.is_abort())
                .await;
            return;
        }
    };

    state.start(producer).await;
    let mut events = SseEventStream::new(response, opts.signal.clone())
        .with_idle_timeout(opts.stream_idle_timeout());
    loop {
        let Some(next) = events.next_event_before(state.flush_deadline()).await else {
            state.flush_pending(producer).await;
            continue;
        };
        match next {
            Ok(Some(event)) if event.data.trim() == "[DONE]" => {
                state.finish(producer, model).await;
                return;
            }
            // Named keepalives and empty data frames carry no JSON payload.
            // Their bytes still refresh the shared SSE idle watchdog.
            Ok(Some(event)) if event.data.trim().is_empty() => continue,
            Ok(Some(event)) => match serde_json::from_str::<Value>(&event.data) {
                Ok(chunk) => {
                    if let Some(message) = chunk
                        .get("error")
                        .and_then(|v| v.get("message"))
                        .and_then(Value::as_str)
                    {
                        state.error(producer, message.to_string(), false).await;
                        return;
                    }
                    state.apply_chunk(producer, &chunk).await;
                }
                Err(error) => {
                    state
                        .error(
                            producer,
                            format!("failed to parse OpenAI SSE chunk: {error}"),
                            false,
                        )
                        .await;
                    return;
                }
            },
            Ok(None) => {
                state.finish(producer, model).await;
                return;
            }
            Err(error) => {
                state
                    .error(producer, error.to_string(), error.is_abort())
                    .await;
                return;
            }
        }
    }
}

fn build_request(model: &Model, ctx: &Context, opts: &SimpleStreamOptions) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = ctx.system_prompt.as_deref().filter(|s| !s.is_empty()) {
        let role = if model.reasoning && supports_developer_role(model) {
            "developer"
        } else {
            "system"
        };
        messages.push(json!({ "role": role, "content": system }));
    }
    let mut pending_tool_images = Vec::new();
    let mut last_was_tool_result = false;
    for message in &ctx.messages {
        if !matches!(message, Message::ToolResult(_)) {
            flush_tool_result_images(model, &mut messages, &mut pending_tool_images);
        }
        match message {
            Message::User(message) => {
                if last_was_tool_result && requires_assistant_after_tool_result(model) {
                    messages.push(json!({
                        "role": "assistant",
                        "content": "I have processed the tool results.",
                    }));
                }
                messages.push(json!({
                    "role": "user",
                    "content": user_content(&message.content),
                }));
                last_was_tool_result = false;
            }
            Message::Assistant(message) => {
                let assistant_content = if requires_thinking_as_text(model) {
                    let parts: Vec<Value> = message
                        .content
                        .iter()
                        .filter_map(|content| match content {
                            Content::Thinking(thinking) if !thinking.thinking.is_empty() => {
                                Some(json!({ "type": "text", "text": thinking.thinking }))
                            }
                            Content::Text(text) if !text.text.is_empty() => {
                                Some(json!({ "type": "text", "text": text.text }))
                            }
                            _ => None,
                        })
                        .collect();
                    if parts.is_empty() {
                        Value::Null
                    } else {
                        Value::Array(parts)
                    }
                } else {
                    let text = Content::text_only(&message.content, "\n");
                    if text.is_empty() {
                        Value::Null
                    } else {
                        Value::String(text)
                    }
                };
                let tool_calls: Vec<Value> = message
                    .content
                    .iter()
                    .filter_map(|content| match content {
                        Content::ToolCall(call) => {
                            let mut value = json!({
                            "id": call.id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": call.arguments.to_string(),
                            }
                            });
                            if model.provider == "google"
                                && message.provider == model.provider
                                && message.model == model.id
                            {
                                if let Some(signature) = &call.thought_signature {
                                    value["extra_content"] =
                                        json!({"google": {"thought_signature": signature}});
                                }
                            }
                            Some(value)
                        }
                        _ => None,
                    })
                    .collect();
                // Strict OpenAI-compatible gateways (DeepSeek, etc.) reject an
                // assistant message that has neither `content` nor
                // `tool_calls`. An aborted/empty turn yields exactly that, so
                // drop it rather than sending an invalid request.
                if assistant_content.is_null() && tool_calls.is_empty() {
                    last_was_tool_result = false;
                    continue;
                }
                let mut value = json!({
                    "role": "assistant",
                    "content": assistant_content,
                });
                if !tool_calls.is_empty() {
                    value["tool_calls"] = Value::Array(tool_calls);
                }
                if model.provider == "google"
                    && message.provider == model.provider
                    && message.model == model.id
                {
                    let signature = message.content.iter().rev().find_map(|part| match part {
                        Content::Text(text) => text.text_signature.as_deref(),
                        _ => None,
                    });
                    if let Some(signature) = signature {
                        value["extra_content"] =
                            json!({"google": {"thought_signature": signature}});
                    }
                }
                if compat(model).and_then(|compat| compat.requires_reasoning_content) == Some(true)
                {
                    let reasoning: String = message
                        .content
                        .iter()
                        .filter_map(|content| match content {
                            Content::Thinking(thinking) => Some(thinking.thinking.as_str()),
                            _ => None,
                        })
                        .collect();
                    value["reasoning_content"] = Value::String(reasoning);
                }
                messages.push(value);
                last_was_tool_result = false;
            }
            Message::ToolResult(message) => {
                let text = Content::text_only(&message.content, "\n");
                let has_images = message
                    .content
                    .iter()
                    .any(|content| matches!(content, Content::Image(_)));
                let content = if !text.is_empty() {
                    text
                } else if has_images {
                    "(see attached image)".to_string()
                } else {
                    "(no tool output)".to_string()
                };
                let mut value = json!({
                    "role": "tool",
                    "tool_call_id": message.tool_call_id,
                    "content": content,
                });
                if requires_tool_result_name(model) && !message.tool_name.is_empty() {
                    value["name"] = Value::String(message.tool_name.clone());
                }
                messages.push(value);
                if model.input.contains(&InputModality::Image) {
                    pending_tool_images.extend(message.content.iter().filter_map(|content| {
                        match content {
                            Content::Image(image) => Some(json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", image.mime_type, image.data)
                                }
                            })),
                            _ => None,
                        }
                    }));
                }
                last_was_tool_result = true;
            }
        }
    }
    flush_tool_result_images(model, &mut messages, &mut pending_tool_images);

    let mut body = Map::new();
    body.insert("model".into(), Value::String(model.id.clone()));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), Value::Bool(true));
    if supports_streaming_usage(model) {
        body.insert("stream_options".into(), json!({ "include_usage": true }));
    }
    let max_tokens = resolve_max_tokens(model, opts);
    if max_tokens > 0 {
        body.insert(max_tokens_field(model).into(), Value::from(max_tokens));
    }
    if let Some(temperature) = opts.temperature {
        body.insert("temperature".into(), Value::from(temperature));
    }
    if compat(model).and_then(|c| c.thinking_format)
        == Some(crate::model::OpenaiThinkingFormat::Deepseek)
    {
        // An explicitly configured DeepSeek format must also disable the
        // endpoint's default thinking when the catalog marks a model non-reasoning.
        let enabled = model.reasoning && opts.reasoning_level() != ThinkingLevel::Off;
        let can_disable = !matches!(
            model
                .thinking_level_map
                .as_ref()
                .and_then(|map| map.get(&ThinkingLevel::Off)),
            Some(None)
        );
        if enabled || can_disable {
            body.insert(
                "thinking".into(),
                json!({ "type": if enabled { "enabled" } else { "disabled" } }),
            );
        }
    }
    if model.provider == "google" && model.id == "gemini-3.8-flash" && model.reasoning {
        if let Some(level) = reasoning_effort(model, opts.reasoning_level()) {
            body.insert("extra_body".into(), json!({
                "google": { "thinking_config": { "thinking_level": level, "include_thoughts": true } }
            }));
        }
    } else if model.reasoning && supports_reasoning_effort(model) {
        if let Some(effort) = reasoning_effort(model, opts.reasoning_level()) {
            body.insert("reasoning_effort".into(), Value::String(effort));
        }
    }
    if !ctx.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(
                ctx.tools
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.parameters.as_value(),
                            }
                        })
                    })
                    .collect(),
            ),
        );
    }
    if supports_store(model) {
        body.insert("store".into(), Value::Bool(false));
    }
    if let Some(params) = &model.sampling_params {
        for (name, value) in params {
            body.insert(name.clone(), value.clone());
        }
    }
    Value::Object(body)
}

fn flush_tool_result_images(
    model: &Model,
    messages: &mut Vec<Value>,
    pending_images: &mut Vec<Value>,
) {
    if pending_images.is_empty() {
        return;
    }
    if requires_assistant_after_tool_result(model) {
        messages.push(json!({
            "role": "assistant",
            "content": "I have processed the tool results.",
        }));
    }
    let mut content = vec![json!({
        "type": "text",
        "text": "Attached image(s) from tool result:",
    })];
    content.append(pending_images);
    messages.push(json!({ "role": "user", "content": content }));
}

fn user_content(content: &UserContent) -> Value {
    match content {
        UserContent::Text(text) => Value::String(text.clone()),
        UserContent::Blocks(blocks) => Value::Array(
            blocks
                .iter()
                .filter_map(|block| match block {
                    Content::Text(text) => Some(json!({ "type": "text", "text": text.text })),
                    Content::Image(image) => Some(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{};base64,{}", image.mime_type, image.data)
                        }
                    })),
                    _ => None,
                })
                .collect(),
        ),
    }
}

fn chat_completions_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    // Pi-compatible configs commonly use either a host root (append `/v1`)
    // or a provider-specific versioned root such as `/api/coding/v3` (append
    // only `/chat/completions`). Do not turn the latter into `/v3/v1/...`.
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else if base
        .rsplit('/')
        .next()
        .is_some_and(|segment| is_version_segment(segment))
    {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    }
}

fn is_version_segment(segment: &str) -> bool {
    let Some(digits) = segment.strip_prefix('v') else {
        return false;
    };
    let digit_count = digits.chars().take_while(|ch| ch.is_ascii_digit()).count();
    digit_count > 0
        && digits
            .chars()
            .skip(digit_count)
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn compat(model: &Model) -> Option<&crate::model::OpenaiCompletionsCompat> {
    model.compat.as_ref().and_then(|compat| match compat {
        StreamingProtocolCompat::OpenaiCompletions(compat) => Some(compat),
        _ => None,
    })
}

fn supports_streaming_usage(model: &Model) -> bool {
    compat(model)
        .and_then(|compat| compat.supports_usage_in_streaming)
        .unwrap_or(true)
}

fn supports_reasoning_effort(model: &Model) -> bool {
    compat(model)
        .and_then(|compat| compat.supports_reasoning_effort)
        .unwrap_or(model.reasoning)
}

fn supports_store(model: &Model) -> bool {
    compat(model)
        .and_then(|compat| compat.supports_store)
        .unwrap_or(false)
}

fn supports_developer_role(model: &Model) -> bool {
    compat(model)
        .and_then(|compat| compat.supports_developer_role)
        .unwrap_or(false)
}

fn supports_finish_reason(model: &Model) -> bool {
    compat(model)
        .and_then(|compat| compat.supports_finish_reason)
        .unwrap_or(true)
}

fn requires_tool_result_name(model: &Model) -> bool {
    compat(model)
        .and_then(|compat| compat.requires_tool_result_name)
        .unwrap_or(false)
}

fn requires_assistant_after_tool_result(model: &Model) -> bool {
    compat(model)
        .and_then(|compat| compat.requires_assistant_after_tool_result)
        .unwrap_or(false)
}

fn requires_thinking_as_text(model: &Model) -> bool {
    compat(model)
        .and_then(|compat| compat.requires_thinking_as_text)
        .unwrap_or(false)
}

/// The reasoning budget a request with the active thinking level should leave
/// room for, in tokens. `0` when thinking is off.
fn thinking_budget(model: &Model, opts: &SimpleStreamOptions) -> u64 {
    crate::providers::anthropic::build_params::thinking_budget_for_level(
        model,
        opts.reasoning_level(),
        opts.thinking_budgets.as_ref(),
    )
}

/// Whether this model's `max_tokens` covers hidden reasoning tokens too.
fn reasoning_counts_toward_max_tokens(model: &Model) -> bool {
    model.reasoning
        && compat(model)
            .and_then(|compat| compat.reasoning_counts_toward_max_tokens)
            .unwrap_or(false)
}

/// Resolve the `max_tokens` / `max_completion_tokens` value for one request.
///
/// `Model::max_tokens` describes the visible *answer* allowance. Gateways that
/// bill hidden reasoning tokens inside that same budget (DeepSeek's reasoning
/// models, and relays that copy them) would otherwise spend it on the chain of
/// thought: the stream ends with `finish_reason: "length"` and an empty or
/// clipped answer, indistinguishable from an interrupted message. For models
/// that declare [`reasoning_counts_toward_max_tokens`](crate::model::OpenaiCompletionsCompat::reasoning_counts_toward_max_tokens)
/// the request reserves the level's reasoning budget on top of the answer
/// allowance, which is the same shape Anthropic gets from
/// `adjust_max_tokens_for_thinking`.
///
/// A caller-pinned `opts.max_tokens` is sent verbatim: whoever computed it owns
/// the reasoning headroom, so this rule never stacks a second time.
fn resolve_max_tokens(model: &Model, opts: &SimpleStreamOptions) -> u64 {
    let Some(pinned) = opts.max_tokens else {
        let base = model.max_tokens;
        return if reasoning_counts_toward_max_tokens(model) {
            base.saturating_add(thinking_budget(model, opts))
        } else {
            base
        };
    };
    pinned
}

fn max_tokens_field(model: &Model) -> &'static str {
    match compat(model).and_then(|compat| compat.max_tokens_field) {
        Some(MaxTokensField::MaxCompletionTokens) => "max_completion_tokens",
        Some(MaxTokensField::MaxTokens) => "max_tokens",
        None if model.reasoning => "max_completion_tokens",
        None => "max_tokens",
    }
}

fn reasoning_effort(model: &Model, level: ThinkingLevel) -> Option<String> {
    if let Some(map) = &model.thinking_level_map {
        if let Some(mapped) = map.get(&level) {
            return mapped.clone();
        }
    }
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => Some("minimal".into()),
        ThinkingLevel::Low => Some("low".into()),
        ThinkingLevel::Medium => Some("medium".into()),
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => Some("high".into()),
    }
}

fn has_auth_header(headers: &BTreeMap<String, String>) -> bool {
    headers.keys().any(|name| {
        name.eq_ignore_ascii_case("authorization")
            || name.eq_ignore_ascii_case("x-api-key")
            || name.eq_ignore_ascii_case("cf-aig-authorization")
    })
}

fn resolve_api_key(
    provider_key: &Option<String>,
    opts: &SimpleStreamOptions,
    allow_env_api_key: bool,
) -> Option<String> {
    resolve_api_key_with_env(provider_key, opts, allow_env_api_key, || {
        std::env::var("OPENAI_API_KEY")
            .ok()
            .filter(|value| !value.is_empty())
    })
}

fn resolve_api_key_with_env(
    provider_key: &Option<String>,
    opts: &SimpleStreamOptions,
    allow_env_api_key: bool,
    read_env_api_key: impl FnOnce() -> Option<String>,
) -> Option<String> {
    opts.api_key
        .clone()
        .or_else(|| provider_key.clone())
        .or_else(|| allow_env_api_key.then(read_env_api_key).flatten())
}

fn apply_api_key(headers: &mut BTreeMap<String, String>, api_key: Option<String>) {
    if let Some(key) = api_key {
        headers.retain(|name, _| !name.eq_ignore_ascii_case("authorization"));
        headers.insert("authorization".into(), format!("Bearer {key}"));
    }
}

pub(super) mod antigravity_stream;
pub(crate) use antigravity_stream::stream_google_native;
pub mod ollama;

struct ToolState {
    content_index: usize,
    id: String,
    name: String,
    arguments: String,
    thought_signature: Option<String>,
}

struct StreamState {
    output: AssistantMessage,
    text_index: Option<usize>,
    thinking_index: Option<usize>,
    tools: BTreeMap<usize, ToolState>,
    finish_reason: Option<DoneReason>,
    finish_error: Option<String>,
    started: bool,
    /// Text/thinking deltas accumulated but not yet emitted as one event.
    pending_text: String,
    pending_thinking: String,
    /// When a batched delta was last emitted, for [`DELTA_FLUSH_INTERVAL`].
    last_flush: Option<tokio::time::Instant>,
}

impl StreamState {
    fn new(model: &Model) -> Self {
        Self {
            output: AssistantMessage::empty(
                model.api.clone(),
                model.provider.clone(),
                model.id.clone(),
                now_ms(),
            ),
            text_index: None,
            thinking_index: None,
            tools: BTreeMap::new(),
            finish_reason: None,
            finish_error: None,
            started: false,
            pending_text: String::new(),
            pending_thinking: String::new(),
            last_flush: None,
        }
    }

    /// Whether a pending batch should be emitted now: either it has grown past
    /// [`DELTA_FLUSH_BYTES`], or it has been waiting [`DELTA_FLUSH_INTERVAL`].
    fn should_flush(&self, pending: &str) -> bool {
        pending.len() >= DELTA_FLUSH_BYTES
            || self
                .last_flush
                .is_none_or(|last| last.elapsed() >= DELTA_FLUSH_INTERVAL)
    }

    fn flush_deadline(&self) -> Option<tokio::time::Instant> {
        if !self.pending_text.is_empty() || !self.pending_thinking.is_empty() {
            self.last_flush.map(|last| last + DELTA_FLUSH_INTERVAL)
        } else {
            None
        }
    }

    /// Emit whatever text/thinking is batched. Called at every channel boundary
    /// so no delta is ever dropped or reordered.
    async fn flush_pending(&mut self, producer: &mut AssistantMessageEventStreamProducer) {
        self.flush_text(producer).await;
        self.flush_thinking(producer).await;
    }

    async fn flush_text(&mut self, producer: &mut AssistantMessageEventStreamProducer) {
        if self.pending_text.is_empty() {
            return;
        }
        let Some(index) = self.text_index else {
            self.pending_text.clear();
            return;
        };
        let delta = std::mem::take(&mut self.pending_text);
        self.last_flush = Some(tokio::time::Instant::now());
        producer
            .push(AssistantMessageEvent::TextDelta {
                content_index: index,
                delta,
                partial: Arc::new(self.output.clone()),
            })
            .await;
    }

    async fn flush_thinking(&mut self, producer: &mut AssistantMessageEventStreamProducer) {
        if self.pending_thinking.is_empty() {
            return;
        }
        let Some(index) = self.thinking_index else {
            self.pending_thinking.clear();
            return;
        };
        let delta = std::mem::take(&mut self.pending_thinking);
        self.last_flush = Some(tokio::time::Instant::now());
        producer
            .push(AssistantMessageEvent::ThinkingDelta {
                content_index: index,
                delta,
                partial: Arc::new(self.output.clone()),
            })
            .await;
    }

    async fn start(&mut self, producer: &mut AssistantMessageEventStreamProducer) {
        if !self.started {
            self.started = true;
            producer
                .push(AssistantMessageEvent::Start {
                    partial: Arc::new(self.output.clone()),
                })
                .await;
        }
    }

    async fn apply_chunk(
        &mut self,
        producer: &mut AssistantMessageEventStreamProducer,
        chunk: &Value,
    ) {
        if let Some(id) = chunk.get("id").and_then(Value::as_str) {
            self.output.response_id = Some(id.to_string());
        }
        if let Some(model) = chunk.get("model").and_then(Value::as_str) {
            self.output.response_model = Some(model.to_string());
        }
        if let Some(usage) = chunk.get("usage").filter(|v| !v.is_null()) {
            self.output.usage = parse_usage(usage);
        }

        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            producer.commit_metadata(&self.output);
            return;
        };
        if chunk.get("usage").map_or(true, Value::is_null) {
            if let Some(usage) = choice.get("usage").filter(|v| !v.is_null()) {
                self.output.usage = parse_usage(usage);
            }
        }
        let delta = choice.get("delta").unwrap_or(&Value::Null);
        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            self.push_text(producer, text).await;
        }
        if let ("google", Some(index), Some(signature)) = (
            self.output.provider.as_str(),
            self.text_index,
            delta
                .pointer("/extra_content/google/thought_signature")
                .and_then(Value::as_str),
        ) {
            if let Some(Content::Text(text)) = self.output.content.get_mut(index) {
                text.text_signature = Some(signature.to_owned());
            }
        }
        let thinking = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .or_else(|| delta.get("reasoning_text"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        if let Some(thinking) = thinking {
            self.push_thinking(producer, thinking).await;
        }
        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (position, tool) in tool_calls.iter().enumerate() {
                self.push_tool_delta(producer, tool, position).await;
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.output.raw_stop_reason = Some(reason.to_string());
            match reason {
                "stop" | "end" if self.output.provider == "google" && !self.tools.is_empty() => {
                    self.finish_reason = Some(DoneReason::ToolUse)
                }
                "stop" | "end" => self.finish_reason = Some(DoneReason::Stop),
                "length" => self.finish_reason = Some(DoneReason::Length),
                "tool_calls" | "function_call" => self.finish_reason = Some(DoneReason::ToolUse),
                other => {
                    self.finish_error = Some(format!("Provider finish_reason: {other}"));
                }
            }
        }
        producer.commit_metadata(&self.output);
    }

    async fn push_text(&mut self, producer: &mut AssistantMessageEventStreamProducer, delta: &str) {
        // Flush before even the start snapshot can expose buffered thinking.
        self.flush_thinking(producer).await;
        let first_text = self.text_index.is_none();
        let index = match self.text_index {
            Some(index) => index,
            None => {
                let index = self.output.content.len();
                self.output.content.push(Content::text(""));
                self.text_index = Some(index);
                producer
                    .push(AssistantMessageEvent::TextStart {
                        content_index: index,
                        partial: Arc::new(self.output.clone()),
                    })
                    .await;
                index
            }
        };
        if let Some(Content::Text(text)) = self.output.content.get_mut(index) {
            text.text.push_str(delta);
        }
        // Batch: `partial` is a full message clone per event, so one event per
        // SSE chunk makes a long stream quadratic. See `DELTA_FLUSH_BYTES`.
        self.pending_text.push_str(delta);
        if first_text || self.should_flush(&self.pending_text) {
            self.flush_text(producer).await;
        }
    }

    async fn push_thinking(
        &mut self,
        producer: &mut AssistantMessageEventStreamProducer,
        delta: &str,
    ) {
        // Keep channel order strict: text already streamed must be emitted before
        // the first thinking delta of a new block.
        self.flush_text(producer).await;
        let first_thinking = self.thinking_index.is_none();
        let index = match self.thinking_index {
            Some(index) => index,
            None => {
                let index = self.output.content.len();
                self.output.content.push(Content::Thinking(ThinkingContent {
                    kind: ThinkingContentType,
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: false,
                }));
                self.thinking_index = Some(index);
                producer
                    .push(AssistantMessageEvent::ThinkingStart {
                        content_index: index,
                        partial: Arc::new(self.output.clone()),
                    })
                    .await;
                index
            }
        };
        if let Some(Content::Thinking(thinking)) = self.output.content.get_mut(index) {
            thinking.thinking.push_str(delta);
        }
        self.pending_thinking.push_str(delta);
        if first_thinking || self.should_flush(&self.pending_thinking) {
            self.flush_thinking(producer).await;
        }
    }

    async fn push_tool_delta(
        &mut self,
        producer: &mut AssistantMessageEventStreamProducer,
        value: &Value,
        position: usize,
    ) {
        // A tool-call block starts a new channel; flush what is batched so the
        // consumer never sees tool events before the text that preceded them.
        self.flush_pending(producer).await;
        let wire_index = value
            .get("index")
            .and_then(Value::as_u64)
            .map(|index| index as usize)
            .unwrap_or_else(|| {
                if self.output.provider != "google" {
                    return 0;
                }
                value["id"].as_str().map_or(position, |id| {
                    self.tools
                        .iter()
                        .find_map(|(index, tool)| (tool.id == id).then_some(*index))
                        .or_else(|| self.tools.keys().next_back().map(|index| index + 1))
                        .unwrap_or(position)
                })
            });
        if !self.tools.contains_key(&wire_index) {
            let content_index = self.output.content.len();
            self.output
                .content
                .push(Content::tool_call("", "", json!({})));
            self.tools.insert(
                wire_index,
                ToolState {
                    content_index,
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                    thought_signature: None,
                },
            );
            producer
                .push(AssistantMessageEvent::ToolCallStart {
                    content_index,
                    partial: Arc::new(self.output.clone()),
                })
                .await;
        }

        let tool = self
            .tools
            .get_mut(&wire_index)
            .expect("tool state inserted");
        if let Some(id) = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            if self.output.provider != "google" || tool.id != id {
                tool.id.push_str(id);
            }
        }
        if let Some(name) = value
            .get("function")
            .and_then(|v| v.get("name"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            if self.output.provider != "google" || tool.name != name {
                tool.name.push_str(name);
            }
        }
        let arguments = value
            .get("function")
            .and_then(|v| v.get("arguments"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        tool.arguments.push_str(arguments);
        if matches!(
            self.output.provider.as_str(),
            "google" | super::antigravity::ID
        ) {
            if let Some(signature) = value
                .pointer("/extra_content/google/thought_signature")
                .and_then(Value::as_str)
            {
                tool.thought_signature = Some(signature.to_owned());
            }
        }
        if let Some(Content::ToolCall(call)) = self.output.content.get_mut(tool.content_index) {
            call.id = tool.id.clone();
            call.name = tool.name.clone();
            call.arguments = parse_streaming_json(Some(&tool.arguments));
            call.thought_signature = tool.thought_signature.clone();
        }
        if !arguments.is_empty() {
            producer
                .push(AssistantMessageEvent::ToolCallDelta {
                    content_index: tool.content_index,
                    delta: arguments.to_string(),
                    partial: Arc::new(self.output.clone()),
                })
                .await;
        }
    }

    async fn finish(&mut self, producer: &mut AssistantMessageEventStreamProducer, model: &Model) {
        // Terminal boundary: emit the last batched delta before the `*End`
        // events, otherwise the tail of a short final chunk would be visible in
        // the final message but never in the delta stream.
        self.flush_pending(producer).await;
        if let Some(index) = self.text_index {
            let content = match self.output.content.get(index) {
                Some(Content::Text(text)) => text.text.clone(),
                _ => String::new(),
            };
            producer
                .push(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content,
                    partial: Arc::new(self.output.clone()),
                })
                .await;
        }
        if let Some(index) = self.thinking_index {
            let content = match self.output.content.get(index) {
                Some(Content::Thinking(thinking)) => thinking.thinking.clone(),
                _ => String::new(),
            };
            producer
                .push(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content,
                    partial: Arc::new(self.output.clone()),
                })
                .await;
        }
        for tool in self.tools.values() {
            let call = ToolCall {
                kind: ToolCallType,
                id: tool.id.clone(),
                name: tool.name.clone(),
                arguments: parse_streaming_json(Some(&tool.arguments)),
                thought_signature: tool.thought_signature.clone(),
                namespace: None,
                recorded: None,
            };
            if let Some(slot) = self.output.content.get_mut(tool.content_index) {
                *slot = Content::ToolCall(call.clone());
            }
            producer
                .push(AssistantMessageEvent::ToolCallEnd {
                    content_index: tool.content_index,
                    tool_call: call,
                    partial: Arc::new(self.output.clone()),
                })
                .await;
        }
        if let Some(message) = self.finish_error.take() {
            self.error(producer, message, false).await;
            return;
        }
        if self.finish_reason.is_none() && supports_finish_reason(model) {
            self.error(
                producer,
                "Stream ended without finish_reason".to_string(),
                false,
            )
            .await;
            return;
        }
        let reason = self.finish_reason.unwrap_or_else(|| {
            if self.tools.is_empty() {
                DoneReason::Stop
            } else {
                DoneReason::ToolUse
            }
        });
        self.output.stop_reason = reason.into();
        self.output.usage.cost = calculate_cost(&model.cost, &self.output.usage);
        producer
            .push(AssistantMessageEvent::Done {
                reason,
                message: self.output.clone(),
            })
            .await;
    }

    async fn error(
        &mut self,
        producer: &mut AssistantMessageEventStreamProducer,
        message: String,
        aborted: bool,
    ) {
        self.output.stop_reason = if aborted {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        self.output.error_message = Some(message);
        // Surface whatever streamed before the failure; the batched tail would
        // otherwise be lost from the delta stream.
        self.flush_pending(producer).await;
        producer
            .push(AssistantMessageEvent::Error {
                reason: if aborted {
                    ErrorReason::Aborted
                } else {
                    ErrorReason::Error
                },
                error: self.output.clone(),
            })
            .await;
    }
}

fn parse_usage(value: &Value) -> Usage {
    let prompt_tokens = value
        .get("prompt_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = value
        .get("completion_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cache_read = value
        .get("prompt_tokens_details")
        .and_then(|v| v.get("cached_tokens"))
        .and_then(Value::as_i64)
        .or_else(|| value.get("prompt_cache_hit_tokens").and_then(Value::as_i64))
        .or_else(|| value.get("cached_tokens").and_then(Value::as_i64))
        .unwrap_or(0);
    let cache_write = value
        .get("prompt_tokens_details")
        .and_then(|v| v.get("cache_write_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let input = (prompt_tokens - cache_read - cache_write).max(0);
    let reasoning = value
        .get("completion_tokens_details")
        .and_then(|v| v.get("reasoning_tokens"))
        .and_then(Value::as_i64);
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning,
        total_tokens: input + output + cache_read + cache_write,
        cost: Default::default(),
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_stream::create_assistant_message_event_stream;
    use crate::model::OpenaiCompletionsCompat;
    use crate::types::{
        AssistantMessage, AssistantMessageEvent, ImageContent, ImageContentType, Message,
        StopReason, Tool, ToolResultMessage, ToolResultRole, UserMessage,
    };
    use crate::Api;

    /// Drive `apply_chunk` with a plain content delta, the shape the SSE mapper
    /// produces for a mid-stream text chunk.
    fn text_chunk(delta: &str) -> Value {
        json!({ "choices": [{ "delta": { "content": delta }, "finish_reason": null }] })
    }

    fn thinking_chunk(delta: &str) -> Value {
        json!({ "choices": [{ "delta": { "reasoning_content": delta }, "finish_reason": null }] })
    }

    /// The final chunk the provider sends before `[DONE]`. Without a
    /// `finish_reason`, `finish()` synthesizes an error ("Stream ended without
    /// finish_reason") rather than a `Done`.
    fn stop_chunk() -> Value {
        json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] })
    }

    /// Drain every event the producer pushed. `next()` yields `None` after the
    /// terminal `Done`/`Error`, so this always terminates.
    async fn drain(stream: &mut AssistantMessageEventStream) -> Vec<AssistantMessageEvent> {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    }

    fn text_deltas(events: &[AssistantMessageEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                AssistantMessageEvent::TextDelta { delta, .. } => Some(delta.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn first_answer_delta_is_immediate_after_thinking() {
        let model = model();
        let (mut producer, mut stream) = create_assistant_message_event_stream();
        let mut state = StreamState::new(&model);
        state.start(&mut producer).await;

        state.apply_chunk(&mut producer, &thinking_chunk("a")).await;
        state.apply_chunk(&mut producer, &thinking_chunk("b")).await;
        state
            .apply_chunk(&mut producer, &text_chunk("answer"))
            .await;
        assert!(state.pending_thinking.is_empty());
        assert!(state.pending_text.is_empty());
        assert!(state.flush_deadline().is_none());

        state.apply_chunk(&mut producer, &stop_chunk()).await;
        state.finish(&mut producer, &model).await;
        assert_eq!(text_deltas(&drain(&mut stream).await), vec!["answer"]);
    }

    #[tokio::test(start_paused = true)]
    async fn first_delta_is_immediate_and_the_next_has_a_flush_deadline() {
        let model = model();
        let (mut producer, mut stream) = create_assistant_message_event_stream();
        let mut state = StreamState::new(&model);
        state.start(&mut producer).await;

        state.apply_chunk(&mut producer, &text_chunk("a")).await;
        assert!(state.pending_text.is_empty());
        assert!(state.flush_deadline().is_none());

        state.apply_chunk(&mut producer, &text_chunk("b")).await;
        assert_eq!(state.pending_text, "b");
        let deadline = state.flush_deadline().unwrap();
        assert_eq!(deadline - tokio::time::Instant::now(), DELTA_FLUSH_INTERVAL);
        tokio::time::advance(DELTA_FLUSH_INTERVAL).await;
        state.flush_pending(&mut producer).await;

        state.apply_chunk(&mut producer, &stop_chunk()).await;
        state.finish(&mut producer, &model).await;
        assert_eq!(text_deltas(&drain(&mut stream).await), vec!["a", "b"]);
    }

    #[tokio::test]
    async fn event_count_is_bounded_by_the_batch_size_not_the_message_length() {
        // The regression this guards: every event carries a full
        // `Arc<AssistantMessage>` snapshot, so one event per SSE chunk made a
        // long stream quadratic (measured 500 deltas 1.1ms → 8000 deltas 105ms).
        // The event count is the cost driver, so bound *it* deterministically
        // rather than asserting on wall-clock time.
        let model = model();
        let (mut producer, mut stream) = create_assistant_message_event_stream();
        let mut state = StreamState::new(&model);
        let total = 8000usize;
        let produce = async {
            state.start(&mut producer).await;
            for _ in 0..total {
                state.apply_chunk(&mut producer, &text_chunk("x")).await;
            }
            state.finish(&mut producer, &model).await;
        };
        let (_, events) = tokio::join!(produce, drain(&mut stream));
        let batches = text_deltas(&events).len();
        assert!(
            batches <= total / DELTA_FLUSH_BYTES + 5,
            "{total} deltas of 1 byte must coalesce into ~{}/{DELTA_FLUSH_BYTES} events, got {batches}",
            total
        );
        // …and the batching must not drop a single byte.
        assert_eq!(text_deltas(&events).join("").len(), total);
    }

    #[tokio::test]
    async fn batched_text_is_flushed_before_text_end_and_matches_the_final_message() {
        let model = model();
        let (mut producer, mut stream) = create_assistant_message_event_stream();
        let mut state = StreamState::new(&model);
        state.start(&mut producer).await;

        // Deliberately shorter than the flush threshold: the tail must still
        // reach the consumer, via the boundary flush in `finish`.
        let short = "hi";
        assert!(short.len() < DELTA_FLUSH_BYTES);
        state.apply_chunk(&mut producer, &text_chunk(short)).await;
        state.apply_chunk(&mut producer, &stop_chunk()).await;
        state.finish(&mut producer, &model).await;

        let events = drain(&mut stream).await;
        let deltas = text_deltas(&events);
        assert_eq!(deltas, vec![short.to_string()]);

        // Ordering: the delta precedes the `TextEnd` that closes the block.
        let delta_at = events
            .iter()
            .position(|e| matches!(e, AssistantMessageEvent::TextDelta { .. }))
            .expect("a TextDelta was emitted");
        let end_at = events
            .iter()
            .position(|e| matches!(e, AssistantMessageEvent::TextEnd { .. }))
            .expect("a TextEnd was emitted");
        assert!(delta_at < end_at, "delta must precede TextEnd");

        // And the delta stream agrees with the finalized message.
        assert!(
            matches!(&state.output.content[0], Content::Text(t) if t.text == short),
            "final message must hold the same text"
        );
        assert!(matches!(
            events.last(),
            Some(AssistantMessageEvent::Done { .. })
        ));
    }

    #[tokio::test]
    async fn channel_switch_flushes_text_before_the_thinking_block() {
        let model = model();
        let (mut producer, mut stream) = create_assistant_message_event_stream();
        let mut state = StreamState::new(&model);
        state.start(&mut producer).await;

        state.apply_chunk(&mut producer, &text_chunk("abc")).await;
        state
            .apply_chunk(&mut producer, &thinking_chunk("reasoning"))
            .await;
        state.finish(&mut producer, &model).await;

        let events = drain(&mut stream).await;
        let delta_at = events
            .iter()
            .position(|e| matches!(e, AssistantMessageEvent::TextDelta { .. }))
            .expect("text must be flushed when the channel switches");
        let thinking_at = events
            .iter()
            .position(|e| {
                matches!(
                    e,
                    AssistantMessageEvent::ThinkingStart { .. }
                        | AssistantMessageEvent::ThinkingDelta { .. }
                )
            })
            .expect("a thinking event was emitted");
        assert!(
            delta_at < thinking_at,
            "buffered text must be emitted before the first thinking event"
        );
    }

    #[tokio::test]
    async fn thinking_deltas_are_batched_losslessly_too() {
        let model = model();
        let (mut producer, mut stream) = create_assistant_message_event_stream();
        let mut state = StreamState::new(&model);
        state.start(&mut producer).await;

        let total = 500usize;
        for _ in 0..total {
            state.apply_chunk(&mut producer, &thinking_chunk("y")).await;
        }
        state.finish(&mut producer, &model).await;

        let events = drain(&mut stream).await;
        let joined: String = events
            .iter()
            .filter_map(|event| match event {
                AssistantMessageEvent::ThinkingDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(joined.len(), total, "no thinking byte may be dropped");
        let thinking_events = events
            .iter()
            .filter(|e| matches!(e, AssistantMessageEvent::ThinkingDelta { .. }))
            .count();
        assert!(
            thinking_events <= total / DELTA_FLUSH_BYTES + 5,
            "thinking deltas must coalesce too, got {thinking_events}"
        );
    }

    #[tokio::test]
    async fn an_error_flushes_batched_text_before_the_terminal_event() {
        let model = model();
        let (mut producer, mut stream) = create_assistant_message_event_stream();
        let mut state = StreamState::new(&model);
        state.start(&mut producer).await;

        state
            .apply_chunk(&mut producer, &text_chunk("partial"))
            .await;
        state.error(&mut producer, "boom".to_string(), false).await;

        let events = drain(&mut stream).await;
        assert_eq!(text_deltas(&events), vec!["partial".to_string()]);
        assert!(matches!(
            events.last(),
            Some(AssistantMessageEvent::Error { .. })
        ));
    }

    fn model() -> Model {
        let mut model = Model::new(
            "gpt-test",
            "GPT Test",
            Api::OpenaiCompletions,
            "gateway",
            "https://example.test/v1",
        );
        model.max_tokens = 1024;
        model.reasoning = true;
        model
    }

    #[tokio::test]
    async fn prepared_auth_failure_skips_body_and_keeps_terminal_error() {
        let model = model();
        let opts = SimpleStreamOptions::default();
        let prepared = prepare_request(
            &model,
            &Context::default(),
            &opts,
            &None,
            false,
            true,
            |_, _, _| panic!("missing credentials must not build the request"),
        );
        assert!(matches!(&prepared, Err(message) if message == "No API key for provider: gateway"));
        let (mut producer, stream) = create_assistant_message_event_stream();
        run_stream(
            &mut producer,
            reqwest::Client::new(),
            &model,
            prepared,
            &opts,
        )
        .await;
        drop(producer);
        let message = stream.result().await.unwrap();
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(
            message.error_message.as_deref(),
            Some("No API key for provider: gateway")
        );
    }

    #[test]
    fn prepared_request_preserves_header_auth_and_overrides() {
        let mut model = model();
        model.headers = Some(BTreeMap::from([
            ("Authorization".into(), "Bearer model-key".into()),
            ("User-Agent".into(), "fixture-agent".into()),
        ]));
        let opts = SimpleStreamOptions {
            headers: Some(BTreeMap::from([(
                "authorization".into(),
                "Bearer caller-key".into(),
            )])),
            ..Default::default()
        };
        let prepared = prepare_request(
            &model,
            &Context::default(),
            &opts,
            &None,
            false,
            true,
            build_request,
        )
        .unwrap();
        assert_eq!(
            prepared.headers.get("authorization").map(String::as_str),
            Some("Bearer caller-key")
        );
        assert_eq!(
            prepared
                .headers
                .keys()
                .filter(|name| name.eq_ignore_ascii_case("authorization"))
                .count(),
            1
        );
        assert_eq!(
            prepared.headers.get("User-Agent").map(String::as_str),
            Some("fixture-agent")
        );
        assert_eq!(prepared.body["model"], "gpt-test");
    }

    #[test]
    fn prepared_unauthenticated_body_owns_the_original_context_snapshot() {
        let model = model();
        let opts = SimpleStreamOptions::default();
        let mut context = Context::new(vec![Message::User(UserMessage::new(
            "original history ".repeat(4096),
            0,
        ))]);
        context.system_prompt = Some("original system".into());
        let expected = build_request(&model, &context, &opts);
        let prepared =
            prepare_request(&model, &context, &opts, &None, false, false, build_request).unwrap();
        context.messages.clear();
        context.system_prompt = Some("replacement system".into());
        drop(context);
        assert_eq!(prepared.body, expected);
        assert!(!has_auth_header(&prepared.headers));
    }

    #[test]
    fn isolated_custom_provider_never_overwrites_auth_with_openai_env_key() {
        let opts = SimpleStreamOptions::default();
        let env_read = std::cell::Cell::new(false);
        let resolved = resolve_api_key_with_env(&None, &opts, false, || {
            env_read.set(true);
            Some("official-openai-key".into())
        });
        let mut headers = BTreeMap::from([("authorization".into(), "Bearer gateway-key".into())]);
        apply_api_key(&mut headers, resolved);

        assert!(
            !env_read.get(),
            "isolated providers must not read OpenAI env auth"
        );
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer gateway-key")
        );
        assert!(headers
            .values()
            .all(|value| value != "Bearer official-openai-key"));
    }

    #[test]
    fn only_exact_openai_provider_id_inherits_the_environment_key() {
        let official = OpenAiCompletionsProvider::with_models(
            "openai",
            None,
            reqwest::Client::new(),
            Vec::new(),
        );
        let case_variant = OpenAiCompletionsProvider::with_models(
            "OpenAI",
            None,
            reqwest::Client::new(),
            Vec::new(),
        );

        assert!(official.allow_env_api_key);
        assert!(!case_variant.allow_env_api_key);
    }

    #[test]
    fn builds_chat_completions_request() {
        let mut context = Context::new(vec![Message::User(UserMessage::new("hello", 0))]);
        context.system_prompt = Some("system".into());
        context.tools.push(Tool {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: json!({ "type": "object" }).into(),
            constrained_sampling: None,
        });
        let mut model = model();
        model.thinking_level_map = Some(BTreeMap::from([(
            ThinkingLevel::Xhigh,
            Some("xhigh".into()),
        )]));
        let mut opts = SimpleStreamOptions::default();
        opts.reasoning = Some(ThinkingLevel::Xhigh);
        let body = build_request(&model, &context, &opts);
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert_eq!(body["reasoning_effort"], "xhigh");
        assert_eq!(body["max_completion_tokens"], 1024);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn reasoning_that_shares_the_completion_budget_reserves_answer_room() {
        // DeepSeek's reasoning models bill the chain of thought inside
        // `max_tokens`. Sending only the answer allowance made long thinking
        // sessions end with `finish_reason: "length"` and no visible text, so
        // the level's reasoning budget is added on top when the model opts in.
        let mut model = model();
        model.compat = Some(StreamingProtocolCompat::OpenaiCompletions(
            OpenaiCompletionsCompat {
                reasoning_counts_toward_max_tokens: Some(true),
                ..Default::default()
            },
        ));
        let context = Context::new(vec![Message::User(UserMessage::new("hello", 0))]);

        let mut opts = SimpleStreamOptions::default();
        opts.reasoning = Some(ThinkingLevel::High);
        let body = build_request(&model, &context, &opts);
        assert_eq!(body["max_completion_tokens"], 1024 + 16_384);

        // Thinking off keeps the plain answer allowance.
        let mut off = SimpleStreamOptions::default();
        off.reasoning = Some(ThinkingLevel::Off);
        let body = build_request(&model, &context, &off);
        assert_eq!(body["max_completion_tokens"], 1024);

        // A caller-pinned ceiling is sent verbatim: it already carries whatever
        // reasoning headroom its owner wanted.
        let mut pinned = SimpleStreamOptions::default();
        pinned.reasoning = Some(ThinkingLevel::Low);
        pinned.max_tokens = Some(4096);
        let body = build_request(&model, &context, &pinned);
        assert_eq!(body["max_completion_tokens"], 4096);

        // Custom per-level budgets are honoured.
        let mut custom = SimpleStreamOptions::default();
        custom.reasoning = Some(ThinkingLevel::High);
        custom.thinking_budgets = Some(crate::types::ThinkingBudgets {
            high: Some(24_000),
            ..Default::default()
        });
        let body = build_request(&model, &context, &custom);
        assert_eq!(body["max_completion_tokens"], 1024 + 24_000);
    }

    #[test]
    fn models_without_the_shared_budget_flag_keep_their_ceiling() {
        // The compat flag is opt-in: every other gateway keeps sending exactly
        // the configured cap, even with thinking enabled.
        let mut model = model();
        assert!(model.compat.is_none());
        let context = Context::new(vec![Message::User(UserMessage::new("hello", 0))]);
        let mut opts = SimpleStreamOptions::default();
        opts.reasoning = Some(ThinkingLevel::High);
        let body = build_request(&model, &context, &opts);
        assert_eq!(body["max_completion_tokens"], 1024);

        model.compat = Some(StreamingProtocolCompat::OpenaiCompletions(
            OpenaiCompletionsCompat {
                reasoning_counts_toward_max_tokens: Some(false),
                ..Default::default()
            },
        ));
        let body = build_request(&model, &context, &opts);
        assert_eq!(body["max_completion_tokens"], 1024);
    }

    #[test]
    fn drops_empty_aborted_assistant_messages() {
        // A stopped/aborted turn leaves a terminal assistant message with no
        // content and no tool calls in context. DeepSeek (and other strict
        // OpenAI-compatible gateways) reject that shape with
        // "content or tool_calls must be set", so it must be omitted.
        let aborted = AssistantMessage::terminal(
            Api::OpenaiCompletions,
            "gateway",
            "gpt-test",
            StopReason::Aborted,
            "cancelled",
            1,
        );
        let context = Context::new(vec![
            Message::User(UserMessage::new("hello", 0)),
            Message::Assistant(Box::new(aborted)),
            Message::User(UserMessage::new("try again", 2)),
        ]);

        let body = build_request(&model(), &context, &SimpleStreamOptions::default());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "user");
        assert!(messages
            .iter()
            .all(|message| message["role"] != "assistant"));
    }

    #[test]
    fn reasoning_content_replay_is_explicit_and_preserves_all_assistant_turns() {
        let mut model = model();
        let mut assistant =
            AssistantMessage::empty(Api::OpenaiCompletions, "gateway", "gpt-test", 1);
        assistant.content = vec![
            Content::thinking("first reasoning"),
            Content::thinking(" and continuation"),
            Content::text("answer"),
        ];
        let mut plain = assistant.clone();
        plain.content = vec![Content::text("plain answer")];
        let ctx = Context::new(vec![
            Message::Assistant(Box::new(assistant)),
            Message::Assistant(Box::new(plain)),
        ]);
        let ordinary = build_request(&model, &ctx, &SimpleStreamOptions::default());
        assert!(ordinary["messages"][0].get("reasoning_content").is_none());
        assert!(!crate::model::prior_reasoning_is_replayed(&model));

        model.compat = Some(StreamingProtocolCompat::OpenaiCompletions(
            OpenaiCompletionsCompat {
                requires_reasoning_content: Some(true),
                ..Default::default()
            },
        ));
        let replayed = build_request(&model, &ctx, &SimpleStreamOptions::default());
        assert_eq!(replayed["messages"][0]["content"], "answer");
        assert_eq!(
            replayed["messages"][0]["reasoning_content"],
            "first reasoning and continuation"
        );
        assert_eq!(replayed["messages"][1]["reasoning_content"], "");
        assert!(crate::model::prior_reasoning_is_replayed(&model));
    }

    #[test]
    fn pi_deepseek_compat_preserves_reasoning_and_controls_thinking() {
        let mut model = model();
        model.compat = Some(StreamingProtocolCompat::OpenaiCompletions(
            serde_json::from_value(json!({
                "thinkingFormat": "deepseek",
                "requiresReasoningContentOnAssistantMessages": true,
                "supportsReasoningEffort": false
            }))
            .unwrap(),
        ));
        let mut assistant =
            AssistantMessage::empty(Api::OpenaiCompletions, "gateway", "gpt-test", 1);
        assistant.content = vec![
            Content::thinking("retained reasoning"),
            Content::text("answer"),
        ];
        let mut plain = assistant.clone();
        plain.content = vec![Content::text("plain answer")];
        let context = Context::new(vec![
            Message::Assistant(Box::new(assistant)),
            Message::Assistant(Box::new(plain)),
        ]);
        let options = SimpleStreamOptions {
            reasoning: Some(ThinkingLevel::High),
            ..Default::default()
        };
        let request = build_request(&model, &context, &options);
        assert_eq!(request["thinking"]["type"], "enabled");
        assert!(request.get("reasoning_effort").is_none());
        assert_eq!(
            request["messages"][0]["reasoning_content"],
            "retained reasoning"
        );
        assert_eq!(request["messages"][1]["reasoning_content"], "");
        let off = build_request(&model, &context, &SimpleStreamOptions::default());
        assert_eq!(off["thinking"]["type"], "disabled");
        model.thinking_level_map = Some(BTreeMap::from([(ThinkingLevel::Off, None)]));
        assert!(
            build_request(&model, &context, &SimpleStreamOptions::default())
                .get("thinking")
                .is_none()
        );
        model.compat = None;
        assert!(build_request(&model, &context, &options)
            .get("thinking")
            .is_none());
    }

    #[test]
    fn deepseek_format_explicitly_disables_thinking_for_non_reasoning_models() {
        let mut model = model();
        model.reasoning = false;
        model.compat = Some(StreamingProtocolCompat::OpenaiCompletions(
            serde_json::from_value(json!({"thinkingFormat":"deepseek"})).unwrap(),
        ));
        let request = build_request(
            &model,
            &Context::new(vec![]),
            &SimpleStreamOptions::default(),
        );
        assert_eq!(request["thinking"]["type"], "disabled");
        assert!(request.get("reasoning_effort").is_none());
    }

    #[test]
    fn applies_message_compat_and_preserves_tool_result_images() {
        let mut model = model();
        model.input.push(InputModality::Image);
        model.compat = Some(StreamingProtocolCompat::OpenaiCompletions(
            OpenaiCompletionsCompat {
                supports_store: Some(true),
                supports_developer_role: Some(true),
                requires_tool_result_name: Some(true),
                requires_assistant_after_tool_result: Some(true),
                requires_thinking_as_text: Some(true),
                ..Default::default()
            },
        ));
        let mut assistant =
            AssistantMessage::empty(Api::OpenaiCompletions, "gateway", "gpt-test", 1);
        assistant.content = vec![Content::thinking("private"), Content::text("answer")];
        let tool_result = ToolResultMessage {
            role: ToolResultRole,
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            content: vec![Content::Image(ImageContent {
                kind: ImageContentType,
                data: "aW1hZ2U=".into(),
                mime_type: "image/png".into(),
            })],
            details: None,
            usage: None,
            added_tool_names: vec![],
            is_error: false,
            timestamp: 2,
        };
        let context = Context {
            system_prompt: Some("system".into()),
            messages: vec![Message::Assistant(Box::new(assistant)), tool_result.into()],
            tools: vec![],
        };

        let body = build_request(&model, &context, &SimpleStreamOptions::default());
        assert_eq!(body["messages"][0]["role"], "developer");
        assert_eq!(body["messages"][1]["content"][0]["text"], "private");
        assert_eq!(body["messages"][1]["content"][1]["text"], "answer");
        assert_eq!(body["messages"][2]["role"], "tool");
        assert_eq!(body["messages"][2]["name"], "read");
        assert_eq!(body["messages"][2]["content"], "(see attached image)");
        assert_eq!(body["messages"][3]["role"], "assistant");
        assert_eq!(body["messages"][4]["role"], "user");
        assert_eq!(
            body["messages"][4]["content"][1]["image_url"]["url"],
            "data:image/png;base64,aW1hZ2U="
        );
        assert_eq!(body["store"], false);
    }

    #[tokio::test]
    async fn maps_text_tool_calls_and_usage() {
        let model = model();
        let (mut producer, _stream) = create_assistant_message_event_stream();
        let mut state = StreamState::new(&model);
        state.start(&mut producer).await;
        state
            .apply_chunk(
                &mut producer,
                &json!({
                    "id": "chat-1",
                    "model": "gpt-test-actual",
                    "choices": [{"delta": {"content": "hello"}, "finish_reason": null}]
                }),
            )
            .await;
        state
            .apply_chunk(
                &mut producer,
                &json!({
                    "choices": [{"delta": {"tool_calls": [{
                        "index": 0,
                        "id": "call-1",
                        "function": {"name": "read", "arguments": "{\"path\":\"a\"}"}
                    }]}, "finish_reason": "tool_calls"}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
                }),
            )
            .await;
        state.finish(&mut producer, &model).await;
        assert_eq!(state.output.stop_reason, StopReason::ToolUse);
        assert_eq!(state.output.response_id.as_deref(), Some("chat-1"));
        assert_eq!(state.output.usage.total_tokens, 15);
        assert!(matches!(&state.output.content[0], Content::Text(text) if text.text == "hello"));
        assert!(
            matches!(&state.output.content[1], Content::ToolCall(call) if call.name == "read" && call.arguments["path"] == "a")
        );
    }

    #[tokio::test]
    async fn gemini_tool_calls_retain_their_signatures_and_distinct_ids() {
        let mut model = model();
        model.provider = "google".into();
        model.id = "gemini-3.8-flash".into();
        let (mut producer, _stream) = create_assistant_message_event_stream();
        let mut state = StreamState::new(&model);
        state.start(&mut producer).await;
        for (id, path, signature) in [("one", "alpha.txt", "first"), ("two", "beta.txt", "second")]
        {
            let arguments = json!({"path": path}).to_string();
            state
                .apply_chunk(
                    &mut producer,
                    &json!({
                        "choices": [{"delta": {"tool_calls": [{
                            "id": id,
                            "function": {"name": "read", "arguments": arguments},
                            "extra_content": {"google": {"thought_signature": signature}}
                        }]}, "finish_reason": "stop"}]
                    }),
                )
                .await;
        }
        state.finish(&mut producer, &model).await;
        assert_eq!(state.output.stop_reason, StopReason::ToolUse);
        let context = Context::new(vec![Message::Assistant(Box::new(state.output))]);
        let body = build_request(&model, &context, &SimpleStreamOptions::default());
        let calls = body["messages"][0]["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "one");
        assert_eq!(
            calls[0]["extra_content"]["google"]["thought_signature"],
            "first"
        );
        assert_eq!(calls[1]["id"], "two");
        assert_eq!(
            calls[1]["extra_content"]["google"]["thought_signature"],
            "second"
        );
    }

    #[test]
    fn splits_cached_usage_without_double_counting_prompt_tokens() {
        let usage = parse_usage(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_tokens_details": {
                "cached_tokens": 40,
                "cache_write_tokens": 10
            },
            "completion_tokens_details": {"reasoning_tokens": 5},
            "total_tokens": 120
        }));
        assert_eq!(usage.input, 50);
        assert_eq!(usage.cache_read, 40);
        assert_eq!(usage.cache_write, 10);
        assert_eq!(usage.output, 20);
        assert_eq!(usage.reasoning, Some(5));
        assert_eq!(usage.total_tokens, 120);
    }

    #[tokio::test]
    async fn maps_provider_error_finish_reason_to_error() {
        let model = model();
        let (mut producer, _stream) = create_assistant_message_event_stream();
        let mut state = StreamState::new(&model);
        state.start(&mut producer).await;
        state
            .apply_chunk(
                &mut producer,
                &json!({
                    "choices": [{"delta": {}, "finish_reason": "content_filter"}]
                }),
            )
            .await;
        state.finish(&mut producer, &model).await;
        assert_eq!(state.output.stop_reason, StopReason::Error);
        assert_eq!(
            state.output.error_message.as_deref(),
            Some("Provider finish_reason: content_filter")
        );
    }

    #[tokio::test]
    async fn missing_finish_reason_obeys_compat_flag() {
        let mut model = model();
        let (mut producer, _stream) = create_assistant_message_event_stream();
        let mut state = StreamState::new(&model);
        state.start(&mut producer).await;
        state.finish(&mut producer, &model).await;
        assert_eq!(state.output.stop_reason, StopReason::Error);

        model.compat = Some(StreamingProtocolCompat::OpenaiCompletions(
            OpenaiCompletionsCompat {
                supports_finish_reason: Some(false),
                ..Default::default()
            },
        ));
        let (mut producer, _stream) = create_assistant_message_event_stream();
        let mut state = StreamState::new(&model);
        state.start(&mut producer).await;
        state.finish(&mut producer, &model).await;
        assert_eq!(state.output.stop_reason, StopReason::Stop);
    }

    #[test]
    fn builds_endpoint_without_duplicate_v1() {
        assert_eq!(
            chat_completions_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://gateway.test"),
            "https://gateway.test/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://gateway.test/proxy"),
            "https://gateway.test/proxy/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://gateway.test/proxy/"),
            "https://gateway.test/proxy/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://ark.cn-beijing.volces.com/api/coding/v3"),
            "https://ark.cn-beijing.volces.com/api/coding/v3/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://gateway.test/v1beta/"),
            "https://gateway.test/v1beta/chat/completions"
        );
        assert_eq!(
            chat_completions_url("https://gateway.test/custom/chat/completions"),
            "https://gateway.test/custom/chat/completions"
        );
    }
    #[tokio::test]
    async fn cancellation_while_enqueue_waits_keeps_committed_content_and_metadata() {
        use crate::event_stream::create_assistant_message_event_stream_with_limits;
        use tokio_util::sync::CancellationToken;
        let parent = CancellationToken::new();
        let (mut producer, mut stream) =
            create_assistant_message_event_stream_with_limits(1, 4 * 1024 * 1024, &parent);
        let mut state = StreamState::new(&model());
        state.start(&mut producer).await;
        stream.next().await.unwrap();
        let accepted = text_chunk("accepted");
        let (_, start) = tokio::join!(state.apply_chunk(&mut producer, &accepted), stream.next(),);
        assert!(matches!(
            start,
            Some(AssistantMessageEvent::TextStart { .. })
        ));
        // The immediate first delta has now filled the queue.
        state
            .apply_chunk(
                &mut producer,
                &json!({ "id": "accepted-id", "choices": [], "usage": { "prompt_tokens": 42 } }),
            )
            .await;

        let rejected = json!({ "id": "rejected-id", "choices": [{ "delta": { "content": "x".repeat(DELTA_FLUSH_BYTES) } }] });
        let mut blocked = Box::pin(state.apply_chunk(&mut producer, &rejected));
        assert!(futures::poll!(&mut blocked).is_pending());
        parent.cancel();
        blocked.await;
        state.error(&mut producer, "cancelled".into(), true).await;
        let result = stream.result().await.unwrap();
        assert_eq!(result.content, vec![Content::text("accepted")]);
        assert_eq!(result.response_id.as_deref(), Some("accepted-id"));
        assert_eq!(result.usage.input, 42);
        assert_eq!(result.stop_reason, StopReason::Aborted);
    }
}
