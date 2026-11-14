//! Antigravity event mapping backed by the shared completions stream state.

use super::*;
use crate::providers::antigravity;

pub(in crate::providers) struct AntigravityStream {
    pub body: Value,
    pub model: Model,
    pub options: SimpleStreamOptions,
    pub http: reqwest::Client,
    pub source: antigravity::AccountSource,
}

pub(in crate::providers) fn stream_antigravity(
    mut request: AntigravityStream,
) -> AssistantMessageEventStream {
    let (mut producer, stream) =
        create_assistant_message_event_stream_with_cancellation(&request.options.signal);
    request.options.signal = producer.cancellation_token();
    tokio::spawn(async move {
        let mut state = StreamState::new(&request.model);
        if let Err(error) = request.run(&mut state, &mut producer).await {
            state
                .error(&mut producer, error, request.options.signal.is_cancelled())
                .await;
        }
    });
    stream
}

/// Google direct/Vertex and Cloud Code Assist share Gemini response parts,
/// including opaque thought signatures, but not their HTTP envelopes.
pub(crate) fn stream_google_native(
    model: Model,
    mut options: SimpleStreamOptions,
    http: reqwest::Client,
    url: String,
    headers: BTreeMap<String, String>,
    body: Value,
    wrapped: bool,
) -> AssistantMessageEventStream {
    let (mut producer, stream) =
        create_assistant_message_event_stream_with_cancellation(&options.signal);
    options.signal = producer.cancellation_token();
    tokio::spawn(async move {
        let mut state = StreamState::new(&model);
        let result: Result<(), String> = async {
            let mut builder = http.post(url).timeout(options.request_timeout());
            for (name, value) in headers {
                builder = builder.header(name, value);
            }
            let request = super::super::request::PreparedJsonRequest::new(builder, &body)
                .map_err(|error| error.to_string())?;
            let response = retry_provider_request(
                || {
                    let request = request.clone();
                    let signal = options.signal.clone();
                    async move {
                        let response = tokio::select! {
                            response = request.send() => response?,
                            _ = signal.cancelled() => return Err(AiError::Abort { message: "Request aborted".into() }),
                        };
                        if response.status().is_success() {
                            Ok(response)
                        } else {
                            Err(AiError::Http {
                                status: Some(response.status().as_u16()),
                                message: response.text().await.unwrap_or_default(),
                            })
                        }
                    }
                },
                options.max_retries,
                options.max_retry_delay,
                &options.signal,
            ).await.map_err(|error| error.to_string())?;
            state.start(&mut producer).await;
            let mut events = SseEventStream::new(response, options.signal.clone())
                .with_idle_timeout(options.stream_idle_timeout());
            loop {
                let Some(next) = events.next_event_before(state.flush_deadline()).await else {
                    state.flush_pending(&mut producer).await;
                    continue;
                };
                let Some(event) = next.map_err(|error| error.to_string())? else { break };
                if event.data.trim().is_empty() { continue; }
                if event.data.trim() == "[DONE]" { break; }
                let mut chunk: Value = serde_json::from_str(&event.data)
                    .map_err(|error| format!("Invalid Gemini stream event: {error}"))?;
                if let Some(error) = chunk.get("error") {
                    return Err(format!("Gemini stream error: {error}"));
                }
                if !wrapped {
                    chunk = json!({"response": chunk});
                }
                state.apply_antigravity_chunk(&mut producer, &chunk).await?;
            }
            if state.finish_reason.is_none() {
                return Err("Gemini stream ended before a finish reason".into());
            }
            state.finish(&mut producer, &model).await;
            Ok(())
        }.await;
        if let Err(error) = result {
            state
                .error(&mut producer, error, options.signal.is_cancelled())
                .await;
        }
    });
    stream
}

impl AntigravityStream {
    async fn response(&mut self) -> Result<reqwest::Response, String> {
        let mut account = (self.source)(None).await?;
        let mut refreshed = false;
        let mut last_error = String::new();
        for endpoint in [antigravity::ENDPOINT, antigravity::SANDBOX_ENDPOINT] {
            let url = format!("{endpoint}/v1internal:streamGenerateContent?alt=sse");
            loop {
                self.body["project"] = json!(account.project_id);
                let response = self
                    .http
                    .post(&url)
                    .header("user-agent", antigravity::user_agent())
                    .bearer_auth(&account.access_token)
                    .timeout(self.options.request_timeout())
                    .json(&self.body)
                    .send()
                    .await;
                let response = match response {
                    Ok(response) => response,
                    Err(error) => {
                        last_error = format!("Antigravity request failed: {error}");
                        break;
                    }
                };
                let status = response.status();
                if status.is_success() {
                    return Ok(response);
                }
                if status == reqwest::StatusCode::UNAUTHORIZED {
                    if refreshed {
                        return Err(
                            "Antigravity rejected the refreshed token. Sign in again using /auth."
                                .into(),
                        );
                    }
                    account = (self.source)(Some(account.access_token)).await?;
                    refreshed = true;
                    continue;
                }
                let detail = response.json::<Value>().await.ok().and_then(|value| {
                    let message = value["error"]["message"].as_str()?;
                    Some(
                        message
                            .replace(&account.access_token, "[token]")
                            .replace(&account.project_id, "[project]")
                            .chars()
                            .take(280)
                            .collect::<String>(),
                    )
                });
                last_error = format!("Antigravity returned HTTP {status}");
                if let Some(detail) = detail {
                    last_error.push_str(&format!(": {detail}"));
                }
                if !(matches!(status.as_u16(), 404 | 429) || status.is_server_error()) {
                    return Err(last_error);
                }
                break;
            }
        }
        Err(last_error)
    }

    async fn run(
        &mut self,
        state: &mut StreamState,
        producer: &mut AssistantMessageEventStreamProducer,
    ) -> Result<(), String> {
        let signal = self.options.signal.clone();
        let response = tokio::select! {
            result = self.response() => result?,
            _ = signal.cancelled() => return Err("Antigravity request cancelled.".into()),
        };
        state.start(producer).await;
        let mut events = SseEventStream::new(response, signal)
            .with_idle_timeout(self.options.stream_idle_timeout());
        loop {
            let Some(next) = events.next_event_before(state.flush_deadline()).await else {
                state.flush_pending(producer).await;
                continue;
            };
            let Some(event) = next.map_err(|error| error.to_string())? else {
                break;
            };
            if event.data.trim() == "[DONE]" {
                break;
            }
            if event.data.trim().is_empty() {
                continue;
            }
            let chunk: Value = serde_json::from_str(&event.data)
                .map_err(|_| "Invalid Antigravity stream event".to_owned())?;
            state.apply_antigravity_chunk(producer, &chunk).await?;
        }
        state.finish(producer, &self.model).await;
        Ok(())
    }
}

impl StreamState {
    async fn apply_antigravity_chunk(
        &mut self,
        producer: &mut AssistantMessageEventStreamProducer,
        chunk: &Value,
    ) -> Result<(), String> {
        if let Some(error) = chunk.get("error") {
            let status = error["status"].as_str().unwrap_or("unknown status");
            return Err(format!("Antigravity stream error: {status}"));
        }
        let response = &chunk["response"];
        if let Some(id) = response["responseId"].as_str() {
            self.output.response_id = Some(id.to_owned());
        }
        if let Some(block) = response["promptFeedback"]["blockReason"].as_str() {
            return Err(format!("Antigravity blocked the prompt: {block}"));
        }
        let candidate = &response["candidates"][0];
        if let Some(parts) = candidate["content"]["parts"].as_array() {
            for part in parts {
                let signature = part["thoughtSignature"].as_str();
                if let Some(call) = part.get("functionCall") {
                    let index = self.tools.len();
                    let name = call["name"]
                        .as_str()
                        .ok_or("Antigravity returned an unnamed function call")?;
                    let id = call["id"]
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4().simple()));
                    let mut wire = json!({
                        "index": index, "id": id,
                        "function": {"name": name, "arguments": call.get("args").unwrap_or(&json!({})).to_string()},
                    });
                    if let Some(signature) = signature {
                        wire["extra_content"] = json!({"google": {"thought_signature": signature}});
                    }
                    self.push_tool_delta(producer, &wire, index).await;
                } else if part["thought"] == true {
                    if let Some(text) = part["text"].as_str().filter(|text| !text.is_empty()) {
                        self.push_thinking(producer, text).await;
                    }
                    if let Some(Content::Thinking(thinking)) = self
                        .thinking_index
                        .and_then(|index| self.output.content.get_mut(index))
                    {
                        if let Some(signature) = signature {
                            thinking.thinking_signature = Some(signature.to_owned());
                        }
                    }
                } else {
                    if let Some(text) = part["text"].as_str().filter(|text| !text.is_empty()) {
                        self.push_text(producer, text).await;
                    }
                    if let Some(Content::Text(text)) = self
                        .text_index
                        .and_then(|index| self.output.content.get_mut(index))
                    {
                        if let Some(signature) = signature {
                            text.text_signature = Some(signature.to_owned());
                        }
                    }
                }
            }
        }
        if let Some(reason) = candidate["finishReason"].as_str() {
            self.output.raw_stop_reason = Some(reason.to_owned());
            self.finish_reason = match reason {
                "STOP" if !self.tools.is_empty() => Some(DoneReason::ToolUse),
                "STOP" => Some(DoneReason::Stop),
                "MAX_TOKENS" => Some(DoneReason::Length),
                other => return Err(format!("Antigravity stopped the response: {other}")),
            };
        }
        if let Some(usage) = response.get("usageMetadata") {
            let reasoning = usage["thoughtsTokenCount"].as_u64().unwrap_or_default();
            self.output.usage = parse_usage(&json!({
                "prompt_tokens": usage["promptTokenCount"],
                "completion_tokens": usage["candidatesTokenCount"].as_u64().unwrap_or_default() + reasoning,
                "total_tokens": usage["totalTokenCount"],
                "prompt_tokens_details": {"cached_tokens": usage["cachedContentTokenCount"]},
                "completion_tokens_details": {"reasoning_tokens": reasoning},
            }));
        }
        producer.commit_metadata(&self.output);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_stream::create_assistant_message_event_stream;

    #[tokio::test]
    async fn retains_thought_signatures_and_distinct_parallel_calls() {
        let model = antigravity::model();
        let (mut producer, _stream) = create_assistant_message_event_stream();
        let mut state = StreamState::new(&model);
        state.start(&mut producer).await;
        state.apply_antigravity_chunk(&mut producer, &json!({"response": {
            "candidates": [{
                "content": {"parts": [
                    {"thought": true, "text": "Read both files.", "thoughtSignature": "thought"},
                    {"functionCall": {"name": "read", "args": {"path": "alpha.txt"}}, "thoughtSignature": "first"},
                    {"functionCall": {"name": "read", "args": {"path": "beta.txt"}}}
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 10, "thoughtsTokenCount": 2, "candidatesTokenCount": 4, "totalTokenCount": 16}
        }})).await.unwrap();
        state.finish(&mut producer, &model).await;
        assert_eq!(state.output.stop_reason, StopReason::ToolUse);
        assert_eq!(state.output.usage.total_tokens, 16);
        let parts = &state.output.content;
        assert!(
            matches!(&parts[0], Content::Thinking(thought) if thought.thinking_signature.as_deref() == Some("thought"))
        );
        let (Content::ToolCall(first), Content::ToolCall(second)) = (&parts[1], &parts[2]) else {
            panic!("Expected two function calls.");
        };
        assert_ne!(first.id, second.id);
        assert_eq!(first.thought_signature.as_deref(), Some("first"));
        assert_eq!(second.thought_signature, None);
        assert_eq!(first.arguments["path"], "alpha.txt");
        assert_eq!(second.arguments["path"], "beta.txt");
    }
}
