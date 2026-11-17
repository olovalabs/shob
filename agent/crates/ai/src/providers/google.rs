//! Native Gemini GenerateContent and Cloud Code Assist request transports.
use crate::types::{Content, Message, ThinkingLevel, UserContent};
use crate::{Context, Model, Provider, SimpleStreamOptions};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub struct GoogleProvider {
    id: String,
    api_key: Option<String>,
    http: reqwest::Client,
    models: Vec<Model>,
}

impl GoogleProvider {
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
impl Provider for GoogleProvider {
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
    ) -> crate::AssistantMessageEventStream {
        match prepare(model, context, options, self.api_key.as_deref()) {
            Ok((url, headers, body, wrapped)) => super::openai_completions::stream_google_native(
                model.clone(),
                options.clone(),
                self.http.clone(),
                url,
                headers,
                body,
                wrapped,
            ),
            Err(error) => {
                let (mut producer, stream) =
                    crate::event_stream::create_assistant_message_event_stream_with_cancellation(
                        &options.signal,
                    );
                let message = crate::AssistantMessage::terminal(
                    model.api.clone(),
                    model.provider.clone(),
                    model.id.clone(),
                    crate::StopReason::Error,
                    error,
                    0,
                );
                tokio::spawn(async move {
                    producer
                        .push(crate::AssistantMessageEvent::Error {
                            reason: crate::ErrorReason::Error,
                            error: message,
                        })
                        .await;
                });
                stream
            }
        }
    }
}

fn header<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}
fn set(headers: &mut BTreeMap<String, String>, name: &str, value: &str) {
    crate::provider::set_http_header(headers, name, Some(value));
}

fn prepare(
    model: &Model,
    context: &Context,
    options: &SimpleStreamOptions,
    default_key: Option<&str>,
) -> Result<(String, BTreeMap<String, String>, Value, bool), String> {
    let mut headers = model.headers.clone().unwrap_or_default();
    if let Some(extra) = &options.headers {
        for (key, value) in extra {
            set(&mut headers, key, value);
        }
    }
    let key = options.api_key.as_deref().or(default_key);
    let api = model.api.as_str();
    let base = model.base_url.trim_end_matches('/');
    let body = build_request(model, context, options);
    if api == "google-gemini-cli" {
        let credentials = key.and_then(|key| serde_json::from_str::<Value>(key).ok());
        let token = credentials
            .as_ref()
            .and_then(|value| value["token"].as_str())
            .or(key.filter(|key| !key.starts_with('{')));
        if header(&headers, "authorization").is_none() {
            set(
                &mut headers,
                "authorization",
                &format!(
                    "Bearer {}",
                    token.ok_or("Google Gemini CLI requires an access token")?
                ),
            );
        }
        let project = credentials
            .as_ref()
            .and_then(|value| value["projectId"].as_str().or(value["project_id"].as_str()))
            .or_else(|| header(&headers, "x-goog-user-project"))
            .ok_or("Google Gemini CLI requires projectId or x-goog-user-project")?
            .to_owned();
        let base = if base.is_empty() {
            "https://cloudcode-pa.googleapis.com"
        } else {
            base
        };
        return Ok((
            format!("{base}/v1internal:streamGenerateContent?alt=sse"),
            headers,
            json!({"project":project,"model":model.id,"request":body}),
            true,
        ));
    }
    if api == "google-vertex" {
        let location = header(&headers, "x-goog-location").unwrap_or("global");
        let host = if location == "global" {
            "aiplatform.googleapis.com".to_owned()
        } else {
            format!("{location}-aiplatform.googleapis.com")
        };
        let bearer = header(&headers, "authorization").is_some();
        let route = if bearer {
            let project = header(&headers, "x-omp-google-project")
                .or_else(|| header(&headers, "x-goog-user-project"))
                .ok_or("Vertex ADC requires x-omp-google-project or x-goog-user-project")?;
            format!(
                "v1/projects/{project}/locations/{location}/publishers/google/models/{}",
                model.id
            )
        } else {
            set(
                &mut headers,
                "x-goog-api-key",
                key.ok_or("Vertex requires an API key or resolved ADC Authorization header")?,
            );
            format!("v1/publishers/google/models/{}", model.id)
        };
        let url = if base.is_empty() {
            format!("https://{host}/{route}")
        } else if base.contains("/models/") {
            base.to_owned()
        } else if base.ends_with("/v1") || base.ends_with("/v1beta1") {
            format!("{base}/{}", route.strip_prefix("v1/").unwrap_or(&route))
        } else {
            format!("{base}/{route}")
        };
        headers.retain(|name, _| {
            !name.eq_ignore_ascii_case("x-omp-google-project")
                && !name.eq_ignore_ascii_case("x-goog-location")
        });
        return Ok((
            format!("{url}:streamGenerateContent?alt=sse"),
            headers,
            body,
            false,
        ));
    }
    if api != "google-generative-ai" {
        return Err(format!("Unsupported native Google API: {api}"));
    }
    if let Some(key) = key {
        set(&mut headers, "x-goog-api-key", key);
    }
    if header(&headers, "x-goog-api-key").is_none() && header(&headers, "authorization").is_none() {
        return Err("Google Generative AI requires an API key".into());
    }
    let base = if base.is_empty() {
        "https://generativelanguage.googleapis.com/v1beta"
    } else {
        base
    };
    Ok((
        format!("{base}/models/{}:streamGenerateContent?alt=sse", model.id),
        headers,
        body,
        false,
    ))
}

fn plain_part(content: &Content) -> Option<Value> {
    match content {
        Content::Text(text) => Some(json!({"text":text.text})),
        Content::Image(image) => {
            Some(json!({"inlineData":{"mimeType":image.mime_type,"data":image.data}}))
        }
        _ => None,
    }
}
fn build_request(model: &Model, context: &Context, options: &SimpleStreamOptions) -> Value {
    let mut contents: Vec<Value> = Vec::new();
    for message in &context.messages {
        let (role, parts): (&str, Vec<Value>) = match message {
            Message::User(user) => (
                "user",
                match &user.content {
                    UserContent::Text(text) => vec![json!({"text":text})],
                    UserContent::Blocks(blocks) => blocks.iter().filter_map(plain_part).collect(),
                },
            ),
            Message::Assistant(assistant) => {
                let same = assistant.provider == model.provider && assistant.model == model.id;
                let mut first_call = true;
                ("model", assistant.content.iter().filter_map(|block| {
                    let (mut part, signature) = match block {
                        Content::Thinking(thinking) if same && thinking.thinking_signature.is_some() => (json!({"text":thinking.thinking,"thought":true}), thinking.thinking_signature.as_deref()),
                        Content::ToolCall(call) => {
                            let signature = call.thought_signature.as_deref().filter(|_| same).or(first_call.then_some("skip_thought_signature_validator"));
                            first_call = false;
                            (json!({"functionCall":{"id":call.id,"name":call.name,"args":call.arguments}}), signature)
                        },
                        Content::Text(text) => (json!({"text":text.text}), text.text_signature.as_deref().filter(|_| same)),
                        other => (plain_part(other)?, None),
                    };
                    if let Some(signature) = signature { part["thoughtSignature"] = json!(signature); }
                    Some(part)
                }).collect())
            }
            Message::ToolResult(result) => {
                let key = if result.is_error { "error" } else { "output" };
                let mut response = json!({"id":result.tool_call_id,"name":result.tool_name,"response":{(key):Content::text_only(&result.content,"\n")}});
                let images: Vec<_> = result
                    .content
                    .iter()
                    .filter(|block| matches!(block, Content::Image(_)))
                    .filter_map(plain_part)
                    .collect();
                if !images.is_empty() {
                    response["parts"] = json!(images);
                }
                ("user", vec![json!({"functionResponse":response})])
            }
        };
        if parts.is_empty() {
            continue;
        }
        if let Some(previous) = contents
            .last_mut()
            .filter(|previous| previous["role"] == role)
        {
            previous["parts"].as_array_mut().unwrap().extend(parts);
        } else {
            contents.push(json!({"role":role,"parts":parts}));
        }
    }
    let mut generation = json!({"maxOutputTokens":options.max_tokens.unwrap_or(model.max_tokens).min(model.max_tokens)});
    if let Some(temperature) = options.temperature {
        generation["temperature"] = json!(temperature);
    }
    if model.reasoning {
        let level = options.reasoning_level();
        let enabled = level != ThinkingLevel::Off;
        let config = if model.id.contains("gemini-3") {
            let tier = match level {
                ThinkingLevel::Off | ThinkingLevel::Minimal => "MINIMAL",
                ThinkingLevel::Low => "LOW",
                ThinkingLevel::Medium => "MEDIUM",
                _ => "HIGH",
            };
            json!({"includeThoughts":enabled,"thinkingLevel":tier})
        } else {
            let budgets = options.thinking_budgets.unwrap_or_default();
            let budget = match level {
                ThinkingLevel::Off => 0,
                ThinkingLevel::Minimal => budgets.minimal.unwrap_or(512),
                ThinkingLevel::Low => budgets.low.unwrap_or(2048),
                ThinkingLevel::Medium => budgets.medium.unwrap_or(8192),
                ThinkingLevel::High => budgets.high.unwrap_or(16384),
                ThinkingLevel::Xhigh => budgets.xhigh.unwrap_or(24576),
                ThinkingLevel::Max => budgets.max.unwrap_or(32768),
            };
            json!({"includeThoughts":enabled,"thinkingBudget":budget})
        };
        generation["thinkingConfig"] = config;
    }
    let mut body = json!({"contents":contents,"generationConfig":generation});
    if let Some(system) = &context.system_prompt {
        body["systemInstruction"] = json!({"parts":[{"text":system}]});
    }
    if !context.tools.is_empty() {
        body["tools"] = json!([{"functionDeclarations":context.tools.iter().map(|tool| json!({"name":tool.name,"description":tool.description,"parametersJsonSchema":tool.parameters.as_value()})).collect::<Vec<_>>()}]);
    }
    if model.api.as_str() == "google-vertex" {
        body["safetySettings"] = json!([
            "HARM_CATEGORY_HATE_SPEECH",
            "HARM_CATEGORY_DANGEROUS_CONTENT",
            "HARM_CATEGORY_SEXUALLY_EXPLICIT",
            "HARM_CATEGORY_HARASSMENT"
        ]
        .map(|category| json!({"category":category,"threshold":"OFF"})));
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    fn model(api: &str) -> Model {
        serde_json::from_value(json!({"id":"gemini-3-pro","name":"Gemini","api":api,"provider":"google","baseUrl":"","reasoning":true,"input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":100000,"maxTokens":8192})).unwrap()
    }
    #[test]
    fn native_api_uses_generate_content_and_key_header() {
        let (url, headers, body, wrapped) = prepare(
            &model("google-generative-ai"),
            &Context::default(),
            &SimpleStreamOptions::default(),
            Some("key"),
        )
        .unwrap();
        assert_eq!(url,"https://generativelanguage.googleapis.com/v1beta/models/gemini-3-pro:streamGenerateContent?alt=sse");
        assert_eq!(headers["x-goog-api-key"], "key");
        assert!(body.get("contents").is_some());
        assert!(!wrapped);
    }
    #[test]
    fn cli_unpacks_credentials_into_cloud_code_envelope() {
        let (url, headers, body, wrapped) = prepare(
            &model("google-gemini-cli"),
            &Context::default(),
            &SimpleStreamOptions::default(),
            Some(r#"{"token":"token","projectId":"project"}"#),
        )
        .unwrap();
        assert!(url.contains("/v1internal:streamGenerateContent"));
        assert_eq!(headers["authorization"], "Bearer token");
        assert_eq!(body["project"], "project");
        assert!(body["request"]["contents"].is_array());
        assert!(wrapped);
    }
    #[test]
    fn vertex_adc_uses_regional_project_route() {
        let options = SimpleStreamOptions {
            headers: Some(BTreeMap::from([
                ("Authorization".into(), "Bearer token".into()),
                ("x-goog-user-project".into(), "project".into()),
                ("x-goog-location".into(), "us-central1".into()),
            ])),
            ..Default::default()
        };
        let (url, _, _, _) =
            prepare(&model("google-vertex"), &Context::default(), &options, None).unwrap();
        assert_eq!(url,"https://us-central1-aiplatform.googleapis.com/v1/projects/project/locations/us-central1/publishers/google/models/gemini-3-pro:streamGenerateContent?alt=sse");
    }
}

#[cfg(test)]
mod replay_tests {
    use super::*;
    #[test]
    fn signed_tool_calls_and_results_use_native_parts() {
        let model: Model = serde_json::from_value(json!({"id":"gemini-3-pro","name":"Gemini","api":"google-generative-ai","provider":"google","baseUrl":"","reasoning":true,"input":["text"],"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"contextWindow":100000,"maxTokens":8192})).unwrap();
        let mut assistant =
            crate::AssistantMessage::empty(model.api.clone(), "google", "gemini-3-pro", 0);
        assistant.content.push(serde_json::from_value(json!({"type":"toolCall","id":"call1","name":"lookup","arguments":{"q":"rust"},"thoughtSignature":"opaque"})).unwrap());
        let result = serde_json::from_value(json!({"role":"toolResult","toolCallId":"call1","toolName":"lookup","content":[{"type":"text","text":"found"}],"isError":false,"timestamp":0})).unwrap();
        let context = Context {
            messages: vec![
                Message::Assistant(Box::new(assistant)),
                Message::ToolResult(result),
            ],
            ..Default::default()
        };
        let body = build_request(&model, &context, &SimpleStreamOptions::default());
        assert_eq!(
            body["contents"][0]["parts"][0]["thoughtSignature"],
            "opaque"
        );
        assert_eq!(
            body["contents"][0]["parts"][0]["functionCall"]["args"]["q"],
            "rust"
        );
        assert_eq!(
            body["contents"][1]["parts"][0]["functionResponse"]["id"],
            "call1"
        );
        assert_eq!(
            body["contents"][1]["parts"][0]["functionResponse"]["response"]["output"],
            "found"
        );
    }
}
