//! Ollama's native `/api/chat` newline-delimited JSON protocol.
use super::*;
use futures::StreamExt;

pub struct OllamaProvider {
    id: String,
    api_key: Option<String>,
    http: reqwest::Client,
    models: Vec<Model>,
}
impl OllamaProvider {
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
#[async_trait]
impl Provider for OllamaProvider {
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
        let (mut producer, stream) =
            create_assistant_message_event_stream_with_cancellation(&options.signal);
        let mut options = options.clone();
        options.signal = producer.cancellation_token();
        let model = model.clone();
        let http = self.http.clone();
        let api_key = options.api_key.clone().or_else(|| self.api_key.clone());
        let body = request_body(&model, context, &options);
        tokio::spawn(async move {
            let mut state = StreamState::new(&model);
            let result = run(
                &http,
                &model,
                &options,
                api_key.as_deref(),
                body,
                &mut state,
                &mut producer,
            )
            .await;
            if let Err(error) = result {
                state
                    .error(&mut producer, error, options.signal.is_cancelled())
                    .await;
            }
        });
        stream
    }
}
fn endpoint(base: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    let base = if base.is_empty() {
        "https://ollama.com"
    } else {
        base
    };
    format!("{}/api/chat", base.strip_suffix("/api").unwrap_or(base))
}
fn plain(blocks: &[Content]) -> Value {
    let mut value = json!({"content":Content::text_only(blocks,"\n")});
    let images: Vec<_> = blocks
        .iter()
        .filter_map(|block| match block {
            Content::Image(image) => Some(&image.data),
            _ => None,
        })
        .collect();
    if !images.is_empty() {
        value["images"] = json!(images);
    }
    value
}
fn request_body(model: &Model, context: &Context, options: &SimpleStreamOptions) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = &context.system_prompt {
        messages.push(json!({"role":"system","content":system}));
    }
    for message in &context.messages {
        let mut wire = match message {
            Message::User(user) => {
                let mut wire = match &user.content {
                    UserContent::Text(text) => json!({"content":text}),
                    UserContent::Blocks(blocks) => plain(blocks),
                };
                wire["role"] = json!("user");
                wire
            }
            Message::ToolResult(result) => {
                let mut wire = plain(&result.content);
                wire["role"] = json!("tool");
                wire["tool_name"] = json!(result.tool_name);
                wire
            }
            Message::Assistant(assistant) => {
                let mut wire = json!({"role":"assistant","content":Content::text_only(&assistant.content,"\n")});
                let calls: Vec<_> = assistant.content.iter().filter_map(|block| match block { Content::ToolCall(call) => Some(json!({"type":"function","function":{"name":call.name,"arguments":call.arguments}})), _ => None }).collect();
                if !calls.is_empty() {
                    wire["tool_calls"] = json!(calls);
                }
                wire
            }
        };
        // Images are supported only on models declaring vision input.
        if !model.input.contains(&InputModality::Image) {
            wire.as_object_mut().unwrap().remove("images");
        }
        messages.push(wire);
    }
    let mut body = json!({"model":model.id,"messages":messages,"stream":true,"options":{"num_predict":options.max_tokens.unwrap_or(model.max_tokens)}});
    if let Some(temperature) = options.temperature {
        body["options"]["temperature"] = json!(temperature);
    }
    if model.reasoning {
        body["think"] = match options.reasoning_level() {
            ThinkingLevel::Off => json!(false),
            ThinkingLevel::Minimal | ThinkingLevel::Low => json!("low"),
            ThinkingLevel::Medium => json!("medium"),
            ThinkingLevel::Max => json!("max"),
            _ => json!("high"),
        };
    }
    if !context.tools.is_empty() {
        body["tools"] = json!(context.tools.iter().map(|tool| json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.parameters.as_value()}})).collect::<Vec<_>>());
    }
    body
}
async fn apply(
    chunk: &Value,
    state: &mut StreamState,
    producer: &mut AssistantMessageEventStreamProducer,
) -> Result<bool, String> {
    if let Some(error) = chunk["error"].as_str() {
        return Err(format!("Ollama stream error: {error}"));
    }
    let message = &chunk["message"];
    if let Some(thinking) = message["thinking"].as_str().filter(|text| !text.is_empty()) {
        state.push_thinking(producer, thinking).await;
    }
    if let Some(text) = message["content"].as_str().filter(|text| !text.is_empty()) {
        state.push_text(producer, text).await;
    }
    if let Some(calls) = message["tool_calls"].as_array() {
        for (position, call) in calls.iter().enumerate() {
            let function = &call["function"];
            let index = function["index"]
                .as_u64()
                .map(|index| index as usize)
                .unwrap_or(position);
            let arguments = match function.get("arguments") {
                Some(Value::String(text)) => text.clone(),
                Some(value) => value.to_string(),
                None => String::new(),
            };
            let mut wire =
                json!({"index":index,"function":{"name":function["name"],"arguments":arguments}});
            if !state.tools.contains_key(&index) {
                wire["id"] = json!(format!("call_{}", uuid::Uuid::new_v4().simple()));
            }
            state.push_tool_delta(producer, &wire, position).await;
        }
    }
    if chunk["done"] == true {
        let reason = chunk["done_reason"].as_str().unwrap_or("stop");
        state.output.raw_stop_reason = Some(reason.to_owned());
        state.finish_reason = Some(if reason == "length" {
            DoneReason::Length
        } else if !state.tools.is_empty() {
            DoneReason::ToolUse
        } else {
            DoneReason::Stop
        });
        state.output.usage = parse_usage(
            &json!({"prompt_tokens":chunk["prompt_eval_count"],"completion_tokens":chunk["eval_count"],"prompt_tokens_details":{"cached_tokens":chunk["prompt_eval_cached_count"]}}),
        );
        producer.commit_metadata(&state.output);
        return Ok(true);
    }
    Ok(false)
}
async fn run(
    http: &reqwest::Client,
    model: &Model,
    options: &SimpleStreamOptions,
    key: Option<&str>,
    body: Value,
    state: &mut StreamState,
    producer: &mut AssistantMessageEventStreamProducer,
) -> Result<(), String> {
    let mut headers = model.headers.clone().unwrap_or_default();
    if let Some(key) = key {
        crate::provider::set_http_header(
            &mut headers,
            "authorization",
            Some(&format!("Bearer {key}")),
        );
    }
    if let Some(extra) = &options.headers {
        for (name, value) in extra {
            crate::provider::set_http_header(&mut headers, name, Some(value));
        }
    }
    let mut request = http
        .post(endpoint(&model.base_url))
        .timeout(options.request_timeout())
        .json(&body);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = tokio::select! { _ = options.signal.cancelled() => return Err("Ollama request cancelled".into()), response = request.send() => response.map_err(|error| format!("Ollama request failed: {error}"))? };
    if !response.status().is_success() {
        return Err(format!("Ollama returned HTTP {}", response.status()));
    }
    state.start(producer).await;
    let mut bytes = response.bytes_stream();
    let mut pending = Vec::new();
    let mut finished = false;
    loop {
        let next = tokio::select! {
            _ = options.signal.cancelled() => return Err("Ollama request cancelled".into()),
            _ = tokio::time::sleep_until(state.flush_deadline().unwrap_or_else(|| tokio::time::Instant::now() + std::time::Duration::from_secs(86400))), if state.flush_deadline().is_some() => { state.flush_pending(producer).await; continue; },
            next = async {
                let idle = options.stream_idle_timeout();
                if let Some(idle) = idle { tokio::time::timeout(idle,bytes.next()).await.map_err(|_| "Ollama stream idle timeout".to_owned()) } else { Ok(bytes.next().await) }
            } => next?,
        };
        let Some(next) = next else {
            break;
        };
        pending.extend_from_slice(&next.map_err(|error| format!("Ollama stream failed: {error}"))?);
        let mut consumed = 0;
        while let Some(position) = pending[consumed..].iter().position(|byte| *byte == b'\n') {
            let end = consumed + position;
            if !pending[consumed..end].iter().all(u8::is_ascii_whitespace) {
                let chunk: Value = serde_json::from_slice(&pending[consumed..end])
                    .map_err(|error| format!("Invalid Ollama JSONL: {error}"))?;
                finished = apply(&chunk, state, producer).await?;
            }
            consumed = end + 1;
            if finished {
                break;
            }
        }
        pending.drain(..consumed);
        if finished {
            break;
        }
    }
    if !finished && !pending.iter().all(u8::is_ascii_whitespace) {
        let chunk: Value = serde_json::from_slice(&pending)
            .map_err(|error| format!("Invalid Ollama JSONL: {error}"))?;
        finished = apply(&chunk, state, producer).await?;
    }
    if !finished {
        return Err("Ollama stream ended before its done record".into());
    }
    state.finish(producer, model).await;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_endpoint_normalizes_api_suffix() {
        assert_eq!(
            endpoint("https://ollama.com/api/"),
            "https://ollama.com/api/chat"
        );
        assert_eq!(endpoint(""), "https://ollama.com/api/chat");
    }
}

#[cfg(test)]
mod behavior_tests {
    use super::*;
    #[tokio::test]
    async fn native_tool_chunk_maps_arguments_and_usage() {
        let model: Model = serde_json::from_value(json!({"id":"gpt-oss","name":"GPT OSS","api":"ollama-chat","provider":"ollama-cloud","baseUrl":"https://ollama.com","reasoning":true,"input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":100000,"maxTokens":8192})).unwrap();
        let mut state = StreamState::new(&model);
        let (mut producer, _stream) = crate::event_stream::create_assistant_message_event_stream();
        let done = apply(&json!({"message":{"thinking":"checking","tool_calls":[{"function":{"name":"lookup","arguments":{"q":"rust"}}}]},"done":true,"done_reason":"stop","prompt_eval_count":20,"prompt_eval_cached_count":5,"eval_count":7}), &mut state, &mut producer).await.unwrap();
        assert!(done);
        assert_eq!(state.finish_reason, Some(DoneReason::ToolUse));
        assert_eq!(state.output.usage.input, 15);
        assert_eq!(state.output.usage.cache_read, 5);
        assert_eq!(state.output.usage.output, 7);
        let call = state
            .output
            .content
            .iter()
            .find_map(|block| match block {
                Content::ToolCall(call) => Some(call),
                _ => None,
            })
            .unwrap();
        assert!(!call.id.is_empty());
        assert_eq!(call.name, "lookup");
        assert_eq!(call.arguments, json!({"q":"rust"}));
    }
}
