//! Cloud Code Assist streaming transport for the Antigravity Gemini model.
//! Wire format reference: Oh My Pi's google-gemini-cli provider (MIT).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{json, Value};

use super::openai_completions::antigravity_stream::{stream_antigravity, AntigravityStream};
use crate::types::{Content, UserContent};
use crate::{Api, Context, InputModality, Message, Model, Provider, SimpleStreamOptions};

pub const ID: &str = "google-antigravity";
pub const MODEL: &str = "gemini-3.8-flash";
pub const ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const SANDBOX_ENDPOINT: &str = "https://daily-cloudcode-pa.sandbox.googleapis.com";

pub struct Account {
    pub access_token: String,
    pub project_id: String,
}

pub type AccountSource = Arc<
    dyn Fn(Option<String>) -> Pin<Box<dyn Future<Output = Result<Account, String>> + Send>>
        + Send
        + Sync,
>;

fn descriptor(id: &str, name: &str, context_window: u64, max_tokens: u64) -> Model {
    let mut model = Model::new(id, name, Api::GoogleGenerativeAi, ID, ENDPOINT);
    model.reasoning = true;
    model.input.push(InputModality::Image);
    model.context_window = context_window;
    model.max_tokens = max_tokens;
    model
}

/// Every Antigravity model this client can address. Gemini 3.8 Flash is the
/// default; the Flash-Lite and Claude routes stay responsive when the 3.8
/// endpoint is congested.
pub fn models() -> Vec<Model> {
    vec![
        descriptor(MODEL, "Gemini 3.8 Flash (Antigravity)", 1_048_576, 65_536),
        descriptor(
            "gemini-3.5-flash-lite",
            "Gemini 3.5 Flash-Lite (Antigravity)",
            1_048_576,
            65_536,
        ),
        descriptor(
            "gemini-3.1-flash-lite",
            "Gemini 3.1 Flash-Lite (Antigravity)",
            1_048_576,
            65_536,
        ),
        descriptor(
            "gemini-2.5-flash",
            "Gemini 2.5 Flash (Antigravity)",
            1_048_576,
            65_536,
        ),
        descriptor(
            "gemini-2.5-flash-lite",
            "Gemini 2.5 Flash-Lite (Antigravity)",
            1_048_576,
            65_536,
        ),
        descriptor(
            "claude-sonnet-4-6",
            "Claude Sonnet 4.6 (Antigravity)",
            250_000,
            64_000,
        ),
    ]
}

pub fn model() -> Model {
    descriptor(MODEL, "Gemini 3.8 Flash (Antigravity)", 1_048_576, 65_536)
}

pub fn user_agent() -> String {
    let version = std::env::var("KATHAMO_ANTIGRAVITY_VERSION").unwrap_or_else(|_| "2.31.0".into());
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else {
        std::env::consts::OS
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    };
    format!("antigravity/hub/{version} (aidev_client; os_type={os}; arch={arch}; cl=1005016104)")
}

pub struct AntigravityProvider {
    http: reqwest::Client,
    source: AccountSource,
    model: Vec<Model>,
    trajectory: uuid::Uuid,
    session_id: String,
}

impl AntigravityProvider {
    pub fn new(http: reqwest::Client, source: AccountSource) -> Self {
        Self::with_models(http, source, models())
    }

    pub fn with_models(http: reqwest::Client, source: AccountSource, models: Vec<Model>) -> Self {
        let random = uuid::Uuid::new_v4().as_u128() as i64;
        Self {
            http,
            source,
            model: models,
            trajectory: uuid::Uuid::new_v4(),
            session_id: random.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl Provider for AntigravityProvider {
    fn id(&self) -> &str {
        ID
    }

    fn models(&self) -> &[Model] {
        &self.model
    }

    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> crate::AssistantMessageEventStream {
        let body = build_request(model, context, options, &self.session_id, self.trajectory);
        stream_antigravity(AntigravityStream {
            body,
            model: model.clone(),
            options: options.clone(),
            http: self.http.clone(),
            source: self.source.clone(),
        })
    }
}

fn plain_part(content: &Content) -> Option<Value> {
    match content {
        Content::Text(text) => Some(json!({"text": text.text})),
        Content::Image(image) => {
            Some(json!({"inlineData": {"mimeType": image.mime_type, "data": image.data}}))
        }
        _ => None,
    }
}

fn assistant_parts(message: &crate::AssistantMessage, model: &Model) -> Vec<Value> {
    let same_model = message.provider == model.provider && message.model == model.id;
    let mut first_call = true;
    message
        .content
        .iter()
        .filter_map(|block| {
            let (mut part, signature) = match block {
                Content::Thinking(thought)
                    if same_model && thought.thinking_signature.is_some() =>
                {
                    (
                        json!({"text": thought.thinking, "thought": true}),
                        thought.thinking_signature.as_deref(),
                    )
                }
                Content::Text(text) => (
                    json!({"text": text.text}),
                    text.text_signature.as_deref().filter(|_| same_model),
                ),
                Content::ToolCall(call) => {
                    let signature = call
                        .thought_signature
                        .as_deref()
                        .filter(|_| same_model)
                        .or(first_call.then_some("skip_thought_signature_validator"));
                    first_call = false;
                    (
                        json!({"functionCall": {"name": call.name, "args": call.arguments}}),
                        signature,
                    )
                }
                other => (plain_part(other)?, None),
            };
            if let Some(signature) = signature {
                part["thoughtSignature"] = json!(signature);
            }
            Some(part)
        })
        .collect()
}

fn build_request(
    model: &Model,
    context: &Context,
    options: &SimpleStreamOptions,
    session: &str,
    trajectory: uuid::Uuid,
) -> Value {
    let mut contents: Vec<Value> = Vec::new();
    let mut tool_results = Vec::new();
    for message in &context.messages {
        if !matches!(message, Message::ToolResult(_)) && !tool_results.is_empty() {
            contents.push(json!({"role": "user", "parts": std::mem::take(&mut tool_results)}));
        }
        let (role, parts) = match message {
            Message::User(user) => (
                "user",
                match &user.content {
                    UserContent::Text(text) => vec![json!({"text": text})],
                    UserContent::Blocks(blocks) => blocks.iter().filter_map(plain_part).collect(),
                },
            ),
            Message::Assistant(assistant) => ("model", assistant_parts(assistant, model)),
            Message::ToolResult(result) => {
                let text = Content::text_only(&result.content, "\n");
                let key = if result.is_error { "error" } else { "output" };
                let mut response = json!({"name": result.tool_name, "response": {(key): text}});
                let images: Vec<_> = result
                    .content
                    .iter()
                    .filter(|block| matches!(block, Content::Image(_)))
                    .filter_map(plain_part)
                    .collect();
                if !images.is_empty() {
                    response["parts"] = json!(images);
                }
                tool_results.push(json!({"functionResponse": response}));
                continue;
            }
        };
        if !parts.is_empty() {
            contents.push(json!({"role": role, "parts": parts}));
        }
    }
    if !tool_results.is_empty() {
        contents.push(json!({"role": "user", "parts": tool_results}));
    }
    let level = match options.reasoning_level() {
        crate::ThinkingLevel::Medium => "MEDIUM",
        crate::ThinkingLevel::High | crate::ThinkingLevel::Xhigh | crate::ThinkingLevel::Max => {
            "HIGH"
        }
        _ => "LOW",
    };
    // Gemini 3.8 selects its thinking tier through a suffixed wire id. The
    // older Gemini and Claude routes take the unsuffixed id plus
    // `thinkingConfig`.
    let wire_model = if model.id == MODEL {
        format!("{}-{}", model.id, level.to_ascii_lowercase())
    } else {
        model.id.clone()
    };
    let mut request = json!({
        "sessionId": session,
        "contents": contents,
        "labels": {
            "cascade_id": trajectory.to_string(),
            "last_step_index": context.messages.len().saturating_sub(1).to_string(),
            "request_id": format!("{trajectory}-{}", context.messages.len()),
            "root_cascade_id": trajectory.to_string(),
            "trajectory_id": trajectory.to_string(),
            "used_claude": "false",
            "used_claude_conservative": "false",
            "used_non_gemini_model": "false",
        },
        "generationConfig": {
            "maxOutputTokens": options.max_tokens.unwrap_or(model.max_tokens).min(model.max_tokens),
            "thinkingConfig": {"thinkingLevel": level, "includeThoughts": options.reasoning_level() != crate::ThinkingLevel::Off}
        },
    });
    if let Some(system) = context.system_prompt.as_ref() {
        request["systemInstruction"] = json!({"role":"user", "parts":[{"text":system}]});
    }
    if !context.tools.is_empty() {
        let declarations: Vec<_> = context.tools.iter().map(|tool| json!({
                    "name": tool.name, "description": tool.description, "parameters": schema_for_cloud_code(tool.parameters.as_value())
        })).collect();
        request["tools"] = json!([{"functionDeclarations": declarations}]);
        request["toolConfig"] = json!({"functionCallingConfig": {"mode": "VALIDATED"}});
    }
    let steps = context.messages.len().max(1);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    json!({
        "model": wire_model,
        "requestId": format!("agent/{trajectory}/{now}/{trajectory}/{steps}"),
        "userAgent": "antigravity",
        "request": request,
    })
}

fn schema_for_cloud_code(schema: &Value) -> Value {
    let mut result = serde_json::Map::new();
    for key in [
        "type",
        "description",
        "format",
        "enum",
        "required",
        "minItems",
        "maxItems",
    ] {
        if let Some(value) = schema.get(key) {
            result.insert(key.into(), value.clone());
        }
    }
    if let Some(types) = schema["type"].as_array() {
        if let Some(kind) = types
            .iter()
            .find(|kind| kind.as_str().is_some_and(|name| name != "null"))
        {
            result.insert("type".into(), kind.clone());
        }
    }
    if let Some(properties) = schema["properties"].as_object() {
        let properties: serde_json::Map<_, _> = properties
            .iter()
            .map(|(name, field)| (name.clone(), schema_for_cloud_code(field)))
            .collect();
        result.insert("properties".into(), Value::Object(properties));
    }
    if let Some(items) = schema.get("items").filter(|value| value.is_object()) {
        result.insert("items".into(), schema_for_cloud_code(items));
    }
    Value::Object(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolResultMessage, ToolResultRole};

    #[test]
    fn replays_signed_calls_with_one_grouped_tool_response() {
        let model = model();
        let mut answer = crate::AssistantMessage::empty(model.api.clone(), ID, MODEL, 0);
        for (id, file) in [("first", "alpha.txt"), ("second", "beta.txt")] {
            answer
                .content
                .push(Content::tool_call(id, "read", json!({"path": file})));
        }
        let Content::ToolCall(call) = &mut answer.content[0] else {
            unreachable!()
        };
        call.thought_signature = Some("signature".into());
        let mut context = Context::new(vec![
            crate::UserMessage::new("Read both files", 0).into(),
            answer.into(),
        ]);
        for (id, value) in [("first", "113"), ("second", "127")] {
            context.messages.push(
                ToolResultMessage {
                    role: ToolResultRole,
                    tool_call_id: id.into(),
                    tool_name: "read".into(),
                    content: vec![Content::text(value)],
                    details: None,
                    usage: None,
                    added_tool_names: vec![],
                    is_error: false,
                    timestamp: 0,
                }
                .into(),
            );
        }
        let payload = build_request(
            &model,
            &context,
            &SimpleStreamOptions::default(),
            "123",
            uuid::Uuid::nil(),
        );
        let contents = payload["request"]["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[1]["parts"][0]["thoughtSignature"], "signature");
        assert!(contents[1]["parts"][1]["thoughtSignature"].is_null());
        let results = contents[2]["parts"].as_array().unwrap();
        assert_eq!(results[0]["functionResponse"]["response"]["output"], "113");
        assert_eq!(results[1]["functionResponse"]["response"]["output"], "127");
    }

    #[test]
    fn routes_effort_through_the_wire_model_id() {
        let context = Context::new(vec![crate::UserMessage::new("hi", 0).into()]);
        let models = models();
        let flash = models.iter().find(|model| model.id == MODEL).unwrap();
        let lite = models
            .iter()
            .find(|model| model.id == "gemini-3.1-flash-lite")
            .unwrap();
        let claude = models
            .iter()
            .find(|model| model.id == "claude-sonnet-4-6")
            .unwrap();
        let low = SimpleStreamOptions {
            reasoning: Some(crate::ThinkingLevel::Low),
            ..Default::default()
        };
        let high = SimpleStreamOptions {
            reasoning: Some(crate::ThinkingLevel::High),
            ..Default::default()
        };
        let wire = |model, options| {
            build_request(model, &context, options, "session", uuid::Uuid::nil())["model"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(wire(flash, &low), "gemini-3.8-flash-low");
        assert_eq!(wire(flash, &high), "gemini-3.8-flash-high");
        assert_eq!(wire(lite, &high), "gemini-3.1-flash-lite");
        assert_eq!(wire(claude, &low), "claude-sonnet-4-6");
    }
}
