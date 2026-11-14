//! Image generation API.
//!
//! Port of upstream's `packages/ai/src/images.ts`,
//! `images-api-registry.ts`, `image-models.ts` and the built-in
//! `api/openrouter-images.ts` provider.
//!
//! Upstream ships one built-in image API — OpenRouter's chat-completions
//! image modality (`openrouter-images`) — and a registry so extensions can
//! register more. `generate_images` resolves the API for a model and dispatches
//! to its provider.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::redact::RedactedHeaders;
use crate::types::{ModelCost, ProviderId, Usage, UsageCost};

/// Image generation API id. `OpenrouterImages` is the built-in; `Other`
/// carries extension-defined ids.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ImagesApi {
    OpenrouterImages,
    Other(String),
}

impl ImagesApi {
    pub fn as_str(&self) -> &str {
        match self {
            Self::OpenrouterImages => "openrouter-images",
            Self::Other(s) => s,
        }
    }
    pub fn from_id(id: &str) -> Self {
        match id {
            "openrouter-images" => Self::OpenrouterImages,
            other => Self::Other(other.to_string()),
        }
    }
}

impl std::fmt::Display for ImagesApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a model can emit text, images, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImagesOutputModality {
    Text,
    Image,
}

/// An image-generation model (native `ImagesModel`).
#[derive(Debug, Clone)]
pub struct ImagesModel {
    pub id: String,
    pub api: ImagesApi,
    pub provider: ProviderId,
    pub base_url: String,
    pub output: Vec<ImagesOutputModality>,
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    /// Cost is $ per million tokens (same convention as [`ModelCost`]).
    pub cost: ModelCost,
}

impl ImagesModel {
    /// Convenience constructor for the built-in OpenRouter image API.
    pub fn openrouter(id: &str) -> Self {
        Self {
            id: id.to_string(),
            api: ImagesApi::OpenrouterImages,
            provider: ProviderId::from("openrouter"),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            output: vec![ImagesOutputModality::Text, ImagesOutputModality::Image],
            headers: None,
            cost: ModelCost::default(),
        }
    }
}

/// One input part (native `ImagesInputContent`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImagesInputContent {
    Text { text: String },
    Image { mime_type: String, data: String },
}

/// One output part (native `ImagesOutputContent`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImagesOutputContent {
    Text { text: String },
    Image { mime_type: String, data: String },
}

/// Generation input (native `ImagesContext`).
#[derive(Debug, Clone, Default)]
pub struct ImagesContext {
    pub input: Vec<ImagesInputContent>,
}

impl ImagesContext {
    pub fn text(prompt: impl Into<String>) -> Self {
        Self {
            input: vec![ImagesInputContent::Text {
                text: prompt.into(),
            }],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagesStopReason {
    Stop,
    Error,
    Aborted,
}

/// Generation result (native `AssistantImages`).
#[derive(Debug, Clone)]
pub struct AssistantImages {
    pub api: ImagesApi,
    pub provider: ProviderId,
    pub model: String,
    pub output: Vec<ImagesOutputContent>,
    pub response_id: Option<String>,
    pub usage: Option<Usage>,
    pub stop_reason: ImagesStopReason,
    pub error_message: Option<String>,
    pub timestamp: i64,
}

/// Options for a generation call (native `ImagesOptions`).
#[derive(Clone, Default)]
pub struct ImagesOptions {
    pub api_key: Option<String>,
    pub headers: std::collections::BTreeMap<String, String>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
}

/// `Debug` redacts credentials: `api_key` is shown only as present or absent,
/// and sensitive header values are masked. The destructuring below is
/// exhaustive, so adding a field to the struct fails to compile until it is
/// printed here too.
impl fmt::Debug for ImagesOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            api_key,
            headers,
            timeout_ms,
            max_retries,
            max_retry_delay_ms,
        } = self;
        f.debug_struct("ImagesOptions")
            .field("api_key", &api_key.as_ref().map(|_| "<redacted>"))
            .field("headers", &RedactedHeaders(headers))
            .field("timeout_ms", timeout_ms)
            .field("max_retries", max_retries)
            .field("max_retry_delay_ms", max_retry_delay_ms)
            .finish()
    }
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// A registered image API provider.
#[derive(Clone)]
pub struct ImagesApiProvider {
    pub api: ImagesApi,
    pub generate: Arc<
        dyn Fn(ImagesModel, ImagesContext, ImagesOptions) -> BoxFuture<AssistantImages>
            + Send
            + Sync,
    >,
}

fn registry() -> &'static RwLock<HashMap<String, ImagesApiProvider>> {
    static REGISTRY: OnceLock<RwLock<HashMap<String, ImagesApiProvider>>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut map = HashMap::new();
        map.insert(
            ImagesApi::OpenrouterImages.as_str().to_string(),
            ImagesApiProvider {
                api: ImagesApi::OpenrouterImages,
                generate: Arc::new(|model, context, options| {
                    Box::pin(openrouter_generate_images(model, context, options))
                }),
            },
        );
        RwLock::new(map)
    })
}

/// Register (or replace) an image API provider.
pub fn register_images_api_provider(provider: ImagesApiProvider) {
    if let Ok(mut map) = registry().write() {
        map.insert(provider.api.as_str().to_string(), provider);
    }
}

/// Look up the provider for an API id.
pub fn get_images_api_provider(api: &ImagesApi) -> Option<ImagesApiProvider> {
    registry().read().ok()?.get(api.as_str()).cloned()
}

/// Generate images for `model` (native `generateImages`). Never panics on a
/// provider failure: the error is encoded in [`AssistantImages::stop_reason`]
/// and `error_message`, matching native.
pub async fn generate_images(
    model: ImagesModel,
    context: ImagesContext,
    options: ImagesOptions,
) -> AssistantImages {
    match get_images_api_provider(&model.api) {
        Some(provider) => (provider.generate)(model, context, options).await,
        None => AssistantImages {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            output: Vec::new(),
            response_id: None,
            usage: None,
            stop_reason: ImagesStopReason::Error,
            error_message: Some(format!("No API provider registered for api: {}", model.api)),
            timestamp: now_ms(),
        },
    }
}

// ---------------------------------------------------------------------------
// Built-in: OpenRouter image modality
// ---------------------------------------------------------------------------

async fn openrouter_generate_images(
    model: ImagesModel,
    context: ImagesContext,
    options: ImagesOptions,
) -> AssistantImages {
    let mut out = AssistantImages {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        output: Vec::new(),
        response_id: None,
        usage: None,
        stop_reason: ImagesStopReason::Stop,
        error_message: None,
        timestamp: now_ms(),
    };

    let Some(api_key) = options.api_key.clone() else {
        out.stop_reason = ImagesStopReason::Error;
        out.error_message = Some(format!("No API key for provider: {}", model.provider));
        return out;
    };

    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
    let payload = build_openrouter_payload(&model, &context);
    let idle = options.timeout_ms.map(|ms| ms as i64);
    let client = match crate::http::build_client(&model.base_url, idle, None) {
        Ok(c) => c,
        Err(e) => {
            out.stop_reason = ImagesStopReason::Error;
            out.error_message = Some(e);
            return out;
        }
    };

    let mut request = client
        .post(&url)
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {api_key}"))
        .json(&payload);
    if let Some(headers) = &model.headers {
        for (key, value) in headers {
            request = request.header(key, value);
        }
    }
    for (key, value) in &options.headers {
        request = request.header(key, value);
    }

    let response = match request.send().await {
        Ok(r) => r,
        Err(error) => {
            out.stop_reason = ImagesStopReason::Error;
            out.error_message = Some(format!("Image request failed: {error}"));
            return out;
        }
    };

    let status = response.status();
    let body: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(error) => {
            out.stop_reason = ImagesStopReason::Error;
            out.error_message = Some(format!("invalid image response: {error}"));
            return out;
        }
    };

    if !status.is_success() {
        out.stop_reason = ImagesStopReason::Error;
        let message = body
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("request failed");
        out.error_message = Some(format!("{}: {message}", status.as_u16()));
        return out;
    }

    out.response_id = body.get("id").and_then(|v| v.as_str()).map(str::to_string);
    if let Some(usage) = body.get("usage") {
        out.usage = Some(parse_openrouter_usage(usage, &model));
    }

    if let Some(choice) = body.get("choices").and_then(|c| c.get(0)) {
        let message = choice.get("message");
        if let Some(content) = message
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
        {
            if !content.is_empty() {
                out.output.push(ImagesOutputContent::Text {
                    text: content.to_string(),
                });
            }
        }
        if let Some(images) = message
            .and_then(|m| m.get("images"))
            .and_then(|i| i.as_array())
        {
            for image in images {
                let url = image.get("image_url").and_then(|u| {
                    u.as_str()
                        .map(str::to_string)
                        .or_else(|| u.get("url").and_then(|v| v.as_str()).map(str::to_string))
                });
                if let Some(data_url) = url {
                    if let Some((mime, data)) = parse_data_url(&data_url) {
                        out.output.push(ImagesOutputContent::Image {
                            mime_type: mime,
                            data,
                        });
                    }
                }
            }
        }
    }

    out
}

fn build_openrouter_payload(model: &ImagesModel, context: &ImagesContext) -> serde_json::Value {
    let content: Vec<serde_json::Value> = context
        .input
        .iter()
        .map(|item| match item {
            ImagesInputContent::Text { text } => {
                serde_json::json!({ "type": "text", "text": sanitize_surrogates(text) })
            }
            ImagesInputContent::Image { mime_type, data } => serde_json::json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{mime_type};base64,{data}") }
            }),
        })
        .collect();

    let modalities: Vec<&str> = if model.output.contains(&ImagesOutputModality::Text) {
        vec!["image", "text"]
    } else {
        vec!["image"]
    };

    serde_json::json!({
        "model": model.id,
        "messages": [{ "role": "user", "content": content }],
        "stream": false,
        "modalities": modalities,
    })
}

/// Extract `(mime, base64)` from a `data:` URL, or `None` when it isn't one.
fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.split(';').next().unwrap_or("").to_string();
    if mime.is_empty() {
        return None;
    }
    Some((mime, data.to_string()))
}

fn parse_openrouter_usage(raw: &serde_json::Value, model: &ImagesModel) -> Usage {
    let prompt = raw
        .get("prompt_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let completion = raw
        .get("completion_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let details = raw.get("prompt_tokens_details");
    let cached = details
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let cache_write = details
        .and_then(|d| d.get("cache_write_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let cache_read = if cache_write > 0 {
        (cached - cache_write).max(0)
    } else {
        cached
    };
    let input = (prompt - cache_read - cache_write).max(0);
    let output = completion;

    let rates = &model.cost.rates;
    let cost = UsageCost {
        input: rates.input / 1_000_000.0 * input as f64,
        output: rates.output / 1_000_000.0 * output as f64,
        cache_read: rates.cache_read / 1_000_000.0 * cache_read as f64,
        cache_write: rates.cache_write / 1_000_000.0 * cache_write as f64,
        total: 0.0,
    };
    let total = cost.input + cost.output + cost.cache_read + cost.cache_write;

    Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: input + output + cache_read + cache_write,
        cost: UsageCost { total, ..cost },
    }
}

/// Native `sanitizeSurrogates`: drop lone UTF-16 surrogate code points that
/// would make the JSON body invalid.
fn sanitize_surrogates(value: &str) -> String {
    value
        .chars()
        .filter(|c| !(0xD800..=0xDFFF).contains(&(*c as u32)))
        .collect()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_ids_round_trip() {
        assert_eq!(
            ImagesApi::from_id("openrouter-images"),
            ImagesApi::OpenrouterImages
        );
        assert_eq!(
            ImagesApi::from_id("openrouter-images").as_str(),
            "openrouter-images"
        );
        assert_eq!(ImagesApi::from_id("custom").as_str(), "custom");
    }

    #[test]
    fn data_url_parsing() {
        assert_eq!(
            parse_data_url("data:image/png;base64,AAAA"),
            Some(("image/png".to_string(), "AAAA".to_string()))
        );
        assert_eq!(parse_data_url("https://x/y.png"), None);
    }

    #[test]
    fn payload_modalities() {
        let model = ImagesModel::openrouter("google/gemini-image");
        let ctx = ImagesContext::text("a red panda");
        let payload = build_openrouter_payload(&model, &ctx);
        assert_eq!(payload["modalities"][0], "image");
        assert_eq!(payload["stream"], false);
        assert_eq!(payload["messages"][0]["content"][0]["type"], "text");
    }

    #[test]
    fn usage_pricing() {
        let mut model = ImagesModel::openrouter("m");
        model.cost.rates.input = 1_000_000.0; // $1 per token for a round number
        let raw = serde_json::json!({
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "prompt_tokens_details": { "cached_tokens": 2 }
        });
        let usage = parse_openrouter_usage(&raw, &model);
        assert_eq!(usage.input, 8);
        assert_eq!(usage.cache_read, 2);
        assert_eq!(usage.output, 5);
        assert!((usage.cost.input - 8.0).abs() < 1e-9);
    }

    #[test]
    fn unknown_api_reports_error() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on(async {
            let mut model = ImagesModel::openrouter("m");
            model.api = ImagesApi::Other("nope".into());
            generate_images(model, ImagesContext::text("x"), ImagesOptions::default()).await
        });
        assert_eq!(result.stop_reason, ImagesStopReason::Error);
        assert!(result
            .error_message
            .unwrap()
            .contains("No API provider registered"));
    }

    #[test]
    fn options_debug_redacts_api_key_and_sensitive_headers() {
        let options = ImagesOptions {
            api_key: Some("img-SECRET-KEY".into()),
            headers: std::collections::BTreeMap::from([
                ("Authorization".into(), "Bearer IMG-SECRET-BEARER".into()),
                ("X-Tenant".into(), "tenant-visible".into()),
            ]),
            timeout_ms: Some(1500),
            max_retries: Some(2),
            max_retry_delay_ms: Some(900),
        };
        let debug = format!("{options:?}");
        assert!(!debug.contains("SECRET-KEY"), "{debug}");
        assert!(!debug.contains("IMG-SECRET-BEARER"), "{debug}");
        assert!(debug.contains("api_key: Some(\"<redacted>\")"), "{debug}");
        assert!(
            debug.contains("\"X-Tenant\": \"tenant-visible\""),
            "{debug}"
        );
        assert!(debug.contains("timeout_ms: Some(1500)"), "{debug}");
        assert!(debug.contains("max_retries: Some(2)"), "{debug}");
        assert!(debug.contains("max_retry_delay_ms: Some(900)"), "{debug}");
    }
}
