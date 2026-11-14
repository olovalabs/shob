//! Provider-specific credential exchanges around the shared native wire clients.
use crate::provider::set_http_header;
use crate::types::{ErrorReason, Message, StopReason};
use crate::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Context, Model, Provider,
    SimpleStreamOptions,
};
use base64::Engine;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

pub struct RoutedProvider {
    inner: Arc<dyn Provider>,
    http: reqwest::Client,
}
impl RoutedProvider {
    pub fn new(inner: Arc<dyn Provider>, http: reqwest::Client) -> Self {
        Self { inner, http }
    }

    async fn prepare(
        &self,
        model: &mut Model,
        context: &Context,
        options: &mut SimpleStreamOptions,
    ) -> Result<(), String> {
        let raw = options.api_key.as_deref().unwrap_or_default();
        let envelope: Option<Value> = if raw.trim_start().starts_with('{') {
            Some(serde_json::from_str(raw).map_err(|_| "Invalid structured provider credential")?)
        } else {
            None
        };
        let token = match &envelope {
            Some(value) => value["token"]
                .as_str()
                .filter(|token| !token.is_empty())
                .ok_or("Structured provider credential omitted token")?,
            None => raw,
        }
        .to_owned();
        let mut headers = BTreeMap::new();
        for layer in [model.headers.as_ref(), options.headers.as_ref()]
            .into_iter()
            .flatten()
        {
            for (name, value) in layer {
                set_http_header(&mut headers, name, Some(value));
            }
        }
        match model.provider.as_str() {
            "snowflake" => {
                let account = if let Some(envelope) = &envelope {
                    let account = envelope["account"]
                        .as_str()
                        .or_else(|| envelope["enterpriseUrl"].as_str())
                        .ok_or("Snowflake OAuth credential is missing its account")?;
                    Some(snowflake_account_origin(account)?)
                } else {
                    std::env::var("SNOWFLAKE_ACCOUNT")
                        .ok()
                        .map(|account| snowflake_account_origin(&account))
                        .transpose()?
                };
                const PLACEHOLDER: &str = "https://snowflake-account.invalid";
                if model.base_url == PLACEHOLDER
                    || model.base_url.starts_with(&format!("{PLACEHOLDER}/"))
                {
                    model.base_url = format!(
                        "{}{}",
                        account.as_deref().ok_or(
                            "Snowflake account is required; configure SNOWFLAKE_ACCOUNT or sign in"
                        )?,
                        &model.base_url[PLACEHOLDER.len()..]
                    );
                } else if envelope.is_some() {
                    let endpoint = reqwest::Url::parse(&model.base_url)
                        .map_err(|_| "Invalid Snowflake endpoint")?;
                    if account.as_deref() != Some(endpoint.origin().ascii_serialization().as_str())
                    {
                        return Err(
                            "Snowflake OAuth account does not match the model endpoint".into()
                        );
                    }
                }
                if envelope.is_some() {
                    options.api_key = Some(token);
                }
            }
            "cloudflare-ai-gateway" => {
                let account = envelope
                    .as_ref()
                    .and_then(|value| value["accountId"].as_str())
                    .map(str::to_owned)
                    .or_else(|| std::env::var("CLOUDFLARE_ACCOUNT_ID").ok());
                let gateway = envelope
                    .as_ref()
                    .and_then(|value| value["gatewayId"].as_str())
                    .map(str::to_owned)
                    .or_else(|| std::env::var("CLOUDFLARE_GATEWAY_ID").ok());
                if model.base_url.contains("<account>") {
                    model.base_url = model.base_url.replace(
                        "<account>",
                        account
                            .as_deref()
                            .ok_or("Cloudflare account ID is required")?,
                    );
                }
                if model.base_url.contains("<gateway>") {
                    model.base_url = model.base_url.replace(
                        "<gateway>",
                        gateway
                            .as_deref()
                            .ok_or("Cloudflare AI Gateway ID is required")?,
                    );
                }
                if !token.is_empty() {
                    set_http_header(&mut headers, "authorization", None);
                    set_http_header(&mut headers, "x-api-key", None);
                    set_http_header(
                        &mut headers,
                        "cf-aig-authorization",
                        Some(&format!("Bearer {token}")),
                    );
                    options.api_key = None;
                }
            }
            "alibaba-coding-plan" | "alibaba-token-plan" => {
                if let Some(envelope) = &envelope {
                    if let Some(base) = envelope["baseUrl"]
                        .as_str()
                        .or_else(|| envelope["enterpriseUrl"].as_str())
                    {
                        model.base_url = base.to_owned();
                    }
                    options.api_key = Some(token);
                }
            }
            "coreweave" => {
                if !headers
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case("OpenAI-Project"))
                {
                    let project = std::env::var("COREWEAVE_PROJECT")
                        .ok()
                        .or_else(|| std::env::var("WANDB_INFERENCE_PROJECT").ok())
                        .or_else(|| {
                            let project = std::env::var("WANDB_PROJECT").ok()?;
                            if project.contains('/') {
                                Some(project)
                            } else {
                                std::env::var("WANDB_ENTITY")
                                    .ok()
                                    .map(|entity| format!("{entity}/{project}"))
                            }
                        });
                    if let Some(project) = project {
                        set_http_header(&mut headers, "OpenAI-Project", Some(&project));
                    }
                }
            }
            "github-copilot" => {
                if token.is_empty() {
                    return Err("Missing GitHub Copilot token".into());
                }
                let (token, endpoint) = if let Some(envelope) = &envelope {
                    (token, envelope["apiEndpoint"].as_str().map(str::to_owned))
                } else {
                    let response = self
                        .http
                        .get("https://api.github.com/copilot_internal/v2/token")
                        .header("authorization", format!("token {token}"))
                        .header("user-agent", "copilot/1.0.82")
                        .timeout(options.request_timeout())
                        .send()
                        .await
                        .map_err(|error| error.to_string())?;
                    let status = response.status();
                    if !status.is_success() {
                        return Err(format!(
                            "Copilot token exchange returned HTTP {status}: {}",
                            response.text().await.unwrap_or_default()
                        ));
                    }
                    let response: Value =
                        response.json().await.map_err(|error| error.to_string())?;
                    (
                        response["token"]
                            .as_str()
                            .ok_or("Copilot token exchange omitted token")?
                            .to_owned(),
                        response["endpoints"]["api"].as_str().map(str::to_owned),
                    )
                };
                if let Some(endpoint) = endpoint.filter(|_| {
                    model.base_url.is_empty() || model.base_url.contains("githubcopilot.com")
                }) {
                    model.base_url = endpoint;
                }
                for (name, value) in [
                    ("User-Agent", "copilot/1.0.82"),
                    ("Editor-Version", "copilot/1.0.82"),
                    ("Copilot-Harness-Id", "copilot-sdk"),
                    ("Openai-Intent", "conversation-agent"),
                    ("X-GitHub-Api-Version", "2026-08-01"),
                ] {
                    set_http_header(&mut headers, name, Some(value));
                }
                if !options.headers.as_ref().is_some_and(|headers| {
                    headers
                        .keys()
                        .any(|name| name.eq_ignore_ascii_case("Copilot-Integration-Id"))
                }) {
                    let integration = std::env::var("COPILOT_INTEGRATION_ID")
                        .ok()
                        .filter(|value| !value.trim().is_empty() && !value.contains(['\r', '\n']));
                    let enterprise = envelope
                        .as_ref()
                        .and_then(|value| value["enterpriseUrl"].as_str())
                        .is_some();
                    set_http_header(
                        &mut headers,
                        "Copilot-Integration-Id",
                        Some(integration.as_deref().unwrap_or(if enterprise {
                            "copilot-developer-cli"
                        } else {
                            "copilot-chat"
                        })),
                    );
                }
                let initiator = if matches!(
                    context.messages.last(),
                    Some(Message::ToolResult(_) | Message::Assistant(_))
                ) {
                    "agent"
                } else {
                    "user"
                };
                if !headers
                    .keys()
                    .any(|name| name.eq_ignore_ascii_case("X-Initiator"))
                {
                    set_http_header(&mut headers, "X-Initiator", Some(initiator));
                }
                let initiator = headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("X-Initiator"))
                    .map(|(_, value)| value.as_str())
                    .unwrap_or(initiator)
                    .to_owned();
                set_http_header(
                    &mut headers,
                    "X-Interaction-Type",
                    Some(&format!("conversation-{initiator}")),
                );
                set_http_header(&mut headers, "x-api-key", None);
                set_http_header(
                    &mut headers,
                    "authorization",
                    Some(&format!("Bearer {token}")),
                );
                options.api_key = None;
            }
            "gitlab-duo" => {
                if token.is_empty() {
                    return Err("Missing GitLab access token".into());
                }
                let response = self
                    .http
                    .post("https://gitlab.com/api/v4/ai/third_party_agents/direct_access")
                    .bearer_auth(&token)
                    .timeout(options.request_timeout())
                    .json(&json!({"feature_flags": {"DuoAgentPlatformNext": true}}))
                    .send()
                    .await
                    .map_err(|error| error.to_string())?;
                let status = response.status();
                if !status.is_success() {
                    return Err(format!(
                        "GitLab Duo direct access returned HTTP {status}: {}",
                        response.text().await.unwrap_or_default()
                    ));
                }
                let response: Value = response.json().await.map_err(|error| error.to_string())?;
                let token = response["token"]
                    .as_str()
                    .ok_or("GitLab direct access omitted token")?;
                let gateway = response["headers"]
                    .as_object()
                    .ok_or("GitLab direct access omitted headers")?;
                let caller_headers = headers;
                headers = BTreeMap::new();
                for (name, value) in gateway {
                    if let Some(value) = value.as_str() {
                        set_http_header(&mut headers, name, Some(value));
                    }
                }
                for (name, value) in caller_headers {
                    set_http_header(&mut headers, &name, Some(&value));
                }
                set_http_header(&mut headers, "x-api-key", None);
                set_http_header(
                    &mut headers,
                    "authorization",
                    Some(&format!("Bearer {token}")),
                );
                options.api_key = None;
            }
            _ if model.api == crate::Api::OpenaiCodexResponses => {
                if token.is_empty()
                    && !headers
                        .keys()
                        .any(|name| name.eq_ignore_ascii_case("authorization"))
                {
                    return Err("Missing Codex access token".into());
                }
                let claims = jwt_claims(&token);
                let auth = claims
                    .as_ref()
                    .map(|value| &value["https://api.openai.com/auth"]);
                if let Some(account) = envelope
                    .as_ref()
                    .and_then(|value| value["accountId"].as_str())
                    .or_else(|| auth.and_then(|value| value["chatgpt_account_id"].as_str()))
                {
                    if !headers
                        .keys()
                        .any(|name| name.eq_ignore_ascii_case("chatgpt-account-id"))
                    {
                        set_http_header(&mut headers, "chatgpt-account-id", Some(account));
                    }
                }
                if let Some(residency) = auth
                    .and_then(|value| {
                        value["chatgpt_data_residency"]
                            .as_str()
                            .or_else(|| value["chatgpt_compute_residency"].as_str())
                    })
                    .filter(|value| !value.is_empty() && *value != "no_constraint")
                {
                    if !headers
                        .keys()
                        .any(|name| name.eq_ignore_ascii_case("x-openai-internal-codex-residency"))
                    {
                        set_http_header(
                            &mut headers,
                            "x-openai-internal-codex-residency",
                            Some(residency),
                        );
                    }
                }
                set_http_header(&mut headers, "x-api-key", None);
                set_http_header(&mut headers, "OpenAI-Beta", None);
                if !token.is_empty() {
                    set_http_header(
                        &mut headers,
                        "authorization",
                        Some(&format!("Bearer {token}")),
                    );
                }
                set_http_header(&mut headers, "originator", Some("codex_cli_rs"));
                set_http_header(&mut headers, "version", Some("0.159.0"));
                set_http_header(&mut headers, "user-agent", Some("codex_cli_rs/0.159.0"));
                set_http_header(
                    &mut headers,
                    "x-codex-routing-hint",
                    Some(&format!("model={}", model.id)),
                );
                if let Some(session) = &options.session_id {
                    for name in ["session_id", "conversation_id"] {
                        set_http_header(&mut headers, name, Some(session));
                    }
                }
                if model
                    .base_url
                    .trim_end_matches('/')
                    .ends_with("/backend-api")
                {
                    model.base_url = format!("{}/codex", model.base_url.trim_end_matches('/'));
                }
                options.api_key = None;
            }
            _ => {}
        }
        model.headers = None;
        options.headers = Some(headers);
        Ok(())
    }
}
fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}
#[async_trait::async_trait]
impl Provider for RoutedProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn models(&self) -> &[Model] {
        self.inner.models()
    }
    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let mut routed_model = model.clone();
        let mut routed_options = options.clone();
        let preparation = tokio::select! {
            result = self.prepare(&mut routed_model, context, &mut routed_options) => result,
            _ = options.signal.cancelled() => Err("Request aborted".into()),
        };
        if let Err(error) = preparation {
            let (mut producer, stream) =
                crate::event_stream::create_assistant_message_event_stream();
            let mut message = AssistantMessage::empty(
                model.api.clone(),
                model.provider.clone(),
                model.id.clone(),
                0,
            );
            let aborted = options.signal.is_cancelled();
            message.stop_reason = if aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            message.error_message = Some(error);
            producer
                .push(AssistantMessageEvent::Error {
                    reason: if aborted {
                        ErrorReason::Aborted
                    } else {
                        ErrorReason::Error
                    },
                    error: message,
                })
                .await;
            return stream;
        }
        self.inner
            .stream_simple(&routed_model, context, &routed_options)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codex_claims_decode_url_safe_jwt() {
        let claims = json!({"https://api.openai.com/auth": {"chatgpt_account_id": "workspace"}});
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
        assert_eq!(
            jwt_claims(&format!("head.{payload}.signature")).unwrap(),
            claims
        );
        assert!(jwt_claims("not-a-jwt").is_none());
    }
}

#[cfg(test)]
mod codex_wire_tests {
    use super::*;
    use crate::providers::openai_responses::OpenAiResponsesProvider;
    use crate::types::{Content, Schema, Tool, ToolResultMessage, ToolResultRole, UserMessage};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn codex_native_http_tool_roundtrip_preserves_wire_contract() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for turn in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let (headers, body) = loop {
                    let mut chunk = [0u8; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    let Some(boundary) = bytes.windows(4).position(|part| part == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let headers = std::str::from_utf8(&bytes[..boundary]).unwrap();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    if bytes.len() < boundary + 4 + length {
                        continue;
                    }
                    break (
                        headers.to_ascii_lowercase(),
                        serde_json::from_slice::<Value>(
                            &bytes[boundary + 4..boundary + 4 + length],
                        )
                        .unwrap(),
                    );
                };
                assert!(headers.starts_with("post /codex/responses "));
                assert!(headers.contains("authorization: bearer access-token"));
                assert!(headers.contains("chatgpt-account-id: workspace"));
                assert!(!headers.contains("x-api-key:"));
                assert_eq!(body["store"], false);
                assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
                assert!(body.get("max_output_tokens").is_none());
                assert!(body.get("temperature").is_none());
                let frames = if turn == 0 {
                    assert_eq!(body["tools"][0]["name"], "read");
                    vec![
                        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_one","call_id":"call_one","name":"read","arguments":""}}),
                        json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":\"file.rs\"}"}),
                        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_one","call_id":"call_one","name":"read","arguments":"{\"path\":\"file.rs\"}"}}),
                        json!({"type":"response.completed","response":{"status":"completed","output":[]}}),
                    ]
                } else {
                    let input = body["input"].as_array().unwrap();
                    let call = input
                        .iter()
                        .find(|item| item["type"] == "function_call")
                        .unwrap();
                    let result = input
                        .iter()
                        .find(|item| item["type"] == "function_call_output")
                        .unwrap();
                    assert_eq!(call["call_id"], result["call_id"]);
                    assert!(call.get("id").is_none());
                    assert!(result["output"].to_string().contains("file contents"));
                    vec![
                        json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_one","role":"assistant","content":[]}}),
                        json!({"type":"response.output_text.delta","output_index":0,"delta":"Read complete."}),
                        json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_one","role":"assistant","content":[{"type":"output_text","text":"Read complete."}]}}),
                        json!({"type":"response.completed","response":{"status":"completed","output":[]}}),
                    ]
                };
                let response = frames
                    .iter()
                    .map(|frame| format!("data: {frame}\n\n"))
                    .collect::<String>();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
            }
        });
        let model = Model::new(
            "codex-test",
            "Codex",
            crate::Api::OpenaiCodexResponses,
            "openai-codex",
            format!("http://{address}/codex"),
        );
        let http = reqwest::Client::new();
        let inner = Arc::new(OpenAiResponsesProvider::with_models_without_env_api_key(
            "openai-codex",
            None,
            http.clone(),
            vec![model.clone()],
        ));
        let provider = RoutedProvider::new(inner, http);
        let options = SimpleStreamOptions {
            api_key: Some(json!({"token":"access-token","accountId":"workspace"}).to_string()),
            max_tokens: Some(64),
            temperature: Some(0.3),
            ..Default::default()
        };
        let mut context = Context::new(vec![Message::User(UserMessage::new("Read file.rs", 0))]);
        context.tools.push(Tool { name: "read".into(), description: "Read file".into(), parameters: Schema::new(json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]})), constrained_sampling: None });
        let first = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            provider
                .stream_simple(&model, &context, &options)
                .await
                .result(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(first.api, crate::Api::OpenaiCodexResponses);
        assert_eq!(first.stop_reason, StopReason::ToolUse);
        let call = first
            .content
            .iter()
            .find_map(|content| {
                if let Content::ToolCall(call) = content {
                    Some(call.clone())
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(call.arguments, json!({"path":"file.rs"}));
        context.messages.push(Message::Assistant(Box::new(first)));
        context.messages.push(
            ToolResultMessage {
                role: ToolResultRole,
                tool_call_id: call.id,
                tool_name: call.name,
                content: vec![Content::text("file contents")],
                details: None,
                usage: None,
                added_tool_names: Vec::new(),
                is_error: false,
                timestamp: 1,
            }
            .into(),
        );
        let second = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            provider
                .stream_simple(&model, &context, &options)
                .await
                .result(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(second.stop_reason, StopReason::Stop);
        assert!(second.content.iter().any(
            |content| matches!(content, Content::Text(text) if text.text == "Read complete.")
        ));
        server.await.unwrap();
    }
}

#[cfg(test)]
mod gateway_tests {
    use super::*;
    use crate::providers::openai_responses::OpenAiResponsesProvider;

    #[tokio::test]
    async fn cloudflare_routes_structured_credentials_without_upstream_key_leaks() {
        let http = reqwest::Client::new();
        let inner = Arc::new(OpenAiResponsesProvider::with_models_without_env_api_key(
            "cloudflare-ai-gateway",
            None,
            http.clone(),
            vec![],
        ));
        let provider = RoutedProvider::new(inner, http);
        let mut model = Model::new(
            "gpt-test",
            "GPT",
            crate::Api::OpenaiResponses,
            "cloudflare-ai-gateway",
            "https://gateway.ai.cloudflare.com/v1/<account>/<gateway>/openai",
        );
        model.headers = Some(BTreeMap::from([(
            "x-api-key".into(),
            "stale-upstream-key".into(),
        )]));
        let mut options = SimpleStreamOptions {
            api_key: Some(
                json!({"token":"cf-token","accountId":"account","gatewayId":"gateway"}).to_string(),
            ),
            ..Default::default()
        };
        provider
            .prepare(&mut model, &Context::default(), &mut options)
            .await
            .unwrap();
        assert_eq!(
            model.base_url,
            "https://gateway.ai.cloudflare.com/v1/account/gateway/openai"
        );
        assert!(model.headers.is_none());
        assert!(options.api_key.is_none());
        let headers = options.headers.unwrap();
        assert_eq!(
            headers.get("cf-aig-authorization").map(String::as_str),
            Some("Bearer cf-token")
        );
        assert!(!headers
            .keys()
            .any(|name| name.eq_ignore_ascii_case("authorization")
                || name.eq_ignore_ascii_case("x-api-key")));
    }

    #[tokio::test]
    async fn alibaba_regional_key_envelope_changes_only_routing_and_key() {
        let http = reqwest::Client::new();
        let inner = Arc::new(OpenAiResponsesProvider::with_models_without_env_api_key(
            "alibaba-coding-plan",
            None,
            http.clone(),
            vec![],
        ));
        let provider = RoutedProvider::new(inner, http);
        let mut model = Model::new(
            "qwen-test",
            "Qwen",
            crate::Api::OpenaiCompletions,
            "alibaba-coding-plan",
            "https://coding-intl.dashscope.aliyuncs.com/v1",
        );
        let mut options = SimpleStreamOptions { api_key: Some(json!({"token":"sk-regional","enterpriseUrl":"https://coding.dashscope.aliyuncs.com/v1"}).to_string()), ..Default::default() };
        provider
            .prepare(&mut model, &Context::default(), &mut options)
            .await
            .unwrap();
        assert_eq!(model.base_url, "https://coding.dashscope.aliyuncs.com/v1");
        assert_eq!(options.api_key.as_deref(), Some("sk-regional"));
    }
}

#[cfg(test)]
mod copilot_tests {
    use super::*;
    use crate::providers::openai_responses::OpenAiResponsesProvider;

    #[tokio::test]
    async fn copilot_exchanged_key_routes_endpoint_and_preserves_explicit_identity() {
        let http = reqwest::Client::new();
        let inner = Arc::new(OpenAiResponsesProvider::with_models_without_env_api_key(
            "github-copilot",
            None,
            http.clone(),
            vec![],
        ));
        let provider = RoutedProvider::new(inner, http);
        let mut model = Model::new(
            "gpt-test",
            "GPT",
            crate::Api::OpenaiResponses,
            "github-copilot",
            "https://api.githubcopilot.com",
        );
        let mut options = SimpleStreamOptions {
            api_key: Some(json!({"token":"exchanged-token","apiEndpoint":"https://api.business.githubcopilot.com"}).to_string()),
            headers: Some(BTreeMap::from([("Copilot-Integration-Id".into(), "explicit-client".into()), ("X-Initiator".into(), "agent".into())])),
            ..Default::default()
        };
        provider
            .prepare(&mut model, &Context::default(), &mut options)
            .await
            .unwrap();
        assert_eq!(model.base_url, "https://api.business.githubcopilot.com");
        assert!(options.api_key.is_none());
        let headers = options.headers.unwrap();
        let value = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(value("authorization"), Some("Bearer exchanged-token"));
        assert_eq!(value("Copilot-Integration-Id"), Some("explicit-client"));
        assert_eq!(value("X-Interaction-Type"), Some("conversation-agent"));
        assert_eq!(value("X-GitHub-Api-Version"), Some("2026-08-01"));
        assert_eq!(value("x-api-key"), None);
    }
}

fn snowflake_account_origin(account: &str) -> Result<String, String> {
    let account = account.trim().to_ascii_lowercase().replace('_', "-");
    if account.contains('\\') || account.is_empty() {
        return Err("A Snowflake account identifier or HTTPS account URL is required".into());
    }
    let url = if account.contains("://") {
        reqwest::Url::parse(&account).map_err(|_| "Invalid Snowflake account URL")?
    } else {
        if account.contains(['/', '?', '#', '@', ':']) {
            return Err("Invalid Snowflake account identifier".into());
        }
        let host = if account.ends_with(".snowflakecomputing.com") {
            account
        } else {
            format!("{account}.snowflakecomputing.com")
        };
        reqwest::Url::parse(&format!("https://{host}"))
            .map_err(|_| "Invalid Snowflake account identifier")?
    };
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
    {
        return Err("Snowflake account URL must use HTTPS on port 443 without credentials".into());
    }
    let mut host = url
        .host_str()
        .ok_or("Snowflake account URL has no hostname")?
        .to_owned();
    if host == "app.snowflake.com" {
        let segments: Vec<_> = url
            .path_segments()
            .into_iter()
            .flatten()
            .filter(|segment| !segment.is_empty())
            .collect();
        if segments.len() < 2 {
            return Err("Snowsight URL must include organization and account".into());
        }
        static LEGACY_REGION: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r"^[a-z]{2}(?:-gov)?-[a-z]+-\d+$").unwrap()
        });
        if LEGACY_REGION.is_match(segments[0]) {
            return Err("Legacy Snowsight region links do not identify an account hostname".into());
        }
        host = format!("{}-{}.snowflakecomputing.com", segments[0], segments[1]);
    }
    let suffix = ".snowflakecomputing.com";
    let valid_label = |label: &str| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && label
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    };
    if !host.ends_with(suffix) || host.len() <= suffix.len() || !host.split('.').all(valid_label) {
        return Err("Snowflake account must be a snowflakecomputing.com hostname".into());
    }
    Ok(format!("https://{host}"))
}

#[cfg(test)]
mod snowflake_tests {
    use super::*;
    #[test]
    fn account_validation_rejects_credential_receiving_foreign_hosts() {
        assert_eq!(
            snowflake_account_origin("org_account").unwrap(),
            "https://org-account.snowflakecomputing.com"
        );
        assert_eq!(
            snowflake_account_origin("https://app.snowflake.com/org/account").unwrap(),
            "https://org-account.snowflakecomputing.com"
        );
        for value in [
            "https://attacker.test",
            "http://org-account.snowflakecomputing.com",
            "https://org-account.snowflakecomputing.com.attacker.test",
            "https://token@org-account.snowflakecomputing.com",
        ] {
            assert!(snowflake_account_origin(value).is_err(), "{value}");
        }
    }
}
