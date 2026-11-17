//! Provider configuration probes used by the RPC provider form.
//!
//! The probes deliberately use the provider's model-list endpoint rather than
//! generating a completion: fetching `/models` is cheap, read-only, and works
//! for both model discovery and a useful API-key/endpoint connectivity test.

use reqwest::header::{HeaderName, HeaderValue, AUTHORIZATION};
use reqwest::{StatusCode, Url};
use serde::Serialize;
use serde_json::Value;
use std::time::{Duration, Instant};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModel {
    pub id: String,
    pub owned_by: Option<String>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    pub supports_images: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProbeResult {
    pub ok: bool,
    pub status: u16,
    pub latency_ms: u64,
    pub model_count: Option<usize>,
}

pub async fn get_models(
    api: &str,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ProviderModel>, String> {
    let candidates = model_url_candidates(base_url)?;
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| format!("failed to create HTTP client: {error}"))?;
    let headers = request_headers(api, api_key)?;
    let mut last_status = None;

    for endpoint in candidates {
        let response = client
            .get(&endpoint)
            .headers(headers.clone())
            .send()
            .await
            .map_err(|error| request_error(&error))?;
        let status = response.status();
        last_status = Some(status);
        if status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED {
            continue;
        }
        if !status.is_success() {
            return Err(format!("provider returned HTTP {}", status.as_u16()));
        }
        let body = bounded_body(response).await?;
        return parse_models(&body);
    }

    Err(format!(
        "model endpoint not found (last status: {})",
        last_status
            .map(|status| status.as_u16().to_string())
            .unwrap_or_else(|| "none".to_string())
    ))
}

pub async fn test_connection(
    api: &str,
    base_url: &str,
    api_key: &str,
) -> Result<ProviderProbeResult, String> {
    let candidates = model_url_candidates(base_url)?;
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| format!("failed to create HTTP client: {error}"))?;
    let headers = request_headers(api, api_key)?;
    let mut last_status = None;
    for endpoint in candidates {
        let started = Instant::now();
        let response = client
            .get(&endpoint)
            .headers(headers.clone())
            .send()
            .await
            .map_err(|error| request_error(&error))?;
        let status = response.status();
        let latency_ms = started.elapsed().as_millis() as u64;
        last_status = Some(status);
        if status == StatusCode::NOT_FOUND || status == StatusCode::METHOD_NOT_ALLOWED {
            continue;
        }
        if !status.is_success() {
            return Err(format!("provider returned HTTP {}", status.as_u16()));
        }
        let model_count = bounded_body(response)
            .await
            .ok()
            .and_then(|body| parse_models(&body).ok())
            .map(|models| models.len());
        return Ok(ProviderProbeResult {
            ok: true,
            status: status.as_u16(),
            latency_ms,
            model_count,
        });
    }
    Err(format!(
        "model endpoint not found (last status: {})",
        last_status
            .map(|status| status.as_u16().to_string())
            .unwrap_or_else(|| "none".to_string())
    ))
}

fn request_headers(api: &str, api_key: &str) -> Result<reqwest::header::HeaderMap, String> {
    let api = api.trim();
    let api_key = api_key.trim();
    if !matches!(
        api,
        "anthropic-messages" | "openai-completions" | "openai-responses"
    ) {
        return Err("unsupported provider protocol".into());
    }
    if api_key.is_empty() {
        return Err("API key is required".into());
    }
    let mut headers = reqwest::header::HeaderMap::new();
    let (name, value) = if api == "anthropic-messages" {
        (
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_str(api_key).map_err(|_| "invalid API key header".to_string())?,
        )
    } else {
        (
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {api_key}"))
                .map_err(|_| "invalid API key header".to_string())?,
        )
    };
    headers.insert(name, value);
    if api == "anthropic-messages" {
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        );
    }
    Ok(headers)
}

fn model_url_candidates(base_url: &str) -> Result<Vec<String>, String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let parsed = Url::parse(trimmed).map_err(|_| "base URL must be a valid URL".to_string())?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("base URL must use http:// or https://".into());
    }
    if trimmed.is_empty() {
        return Err("base URL is empty".into());
    }
    let mut candidates = vec![format!("{trimmed}/models")];
    if !trimmed.ends_with("/v1") && !trimmed.ends_with("/v2") && !trimmed.ends_with("/v3") {
        candidates.push(format!("{trimmed}/v1/models"));
    }
    Ok(candidates)
}

async fn bounded_body(response: reqwest::Response) -> Result<String, String> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err("model response is too large".into());
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("failed to read model response: {error}"))?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err("model response is too large".into());
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| "model response is not valid UTF-8".into())
}

fn parse_models(body: &str) -> Result<Vec<ProviderModel>, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|_| "failed to parse model response".to_string())?;
    let entries = value
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| value.get("models").and_then(Value::as_array))
        .ok_or_else(|| "model response has no data or models array".to_string())?;
    let mut models = entries
        .iter()
        .filter_map(|entry| {
            let id = entry
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| entry.get("slug").and_then(Value::as_str))
                .or_else(|| entry.get("name").and_then(Value::as_str))?
                .trim();
            if id.is_empty() {
                return None;
            }
            Some(ProviderModel {
                id: id.strip_prefix("models/").unwrap_or(id).to_string(),
                owned_by: entry
                    .get("owned_by")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                context_window: model_context_window(entry),
                max_tokens: model_max_tokens(entry),
                supports_images: model_supports_images(entry),
            })
        })
        .collect::<Vec<_>>();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    Ok(models)
}

fn model_context_window(entry: &Value) -> Option<u64> {
    [
        entry.get("contextWindow"),
        entry.get("context_window"),
        entry.get("context_length"),
        entry.get("max_context_size"),
        entry.get("limit").and_then(|limit| limit.get("context")),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
            .filter(|value| *value > 0)
    })
}

fn model_max_tokens(entry: &Value) -> Option<u64> {
    [
        entry.get("maxTokens"),
        entry.get("max_tokens"),
        entry.get("max_output_tokens"),
        entry.get("output"),
        entry.get("limit").and_then(|limit| limit.get("output")),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| {
        value
            .as_u64()
            .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
            .filter(|value| *value > 0)
    })
}

fn model_supports_images(entry: &Value) -> Option<bool> {
    for key in ["supportsImages", "supports_images", "vision"] {
        if let Some(value) = entry.get(key).and_then(Value::as_bool) {
            return Some(value);
        }
    }
    for key in ["input", "modalities", "input_modalities"] {
        if let Some(values) = entry.get(key).and_then(Value::as_array) {
            return Some(values.iter().any(|value| {
                value
                    .as_str()
                    .is_some_and(|text| matches!(text, "image" | "images" | "vision"))
            }));
        }
    }
    entry
        .get("capabilities")
        .and_then(|value| value.get("image").or_else(|| value.get("vision")))
        .and_then(Value::as_bool)
}

fn request_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "provider request timed out".into()
    } else if error.is_connect() {
        "could not connect to provider".into()
    } else {
        "provider request failed".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_candidates_use_versioned_base_without_duplicate_v1() {
        assert_eq!(
            model_url_candidates("https://example.test/v1").unwrap(),
            vec!["https://example.test/v1/models"]
        );
        assert_eq!(
            model_url_candidates("https://example.test").unwrap(),
            vec![
                "https://example.test/models",
                "https://example.test/v1/models"
            ]
        );
    }

    #[test]
    fn parse_openai_and_anthropic_model_shapes() {
        let models = parse_models(
            r#"{"data":[{"id":"b"},{"id":"a","owned_by":"x","contextWindow":128000,"max_tokens":4096,"input":["text","image"]}]}"#,
        )
        .unwrap();
        assert_eq!(models[0].id, "a");
        assert_eq!(models[0].context_window, Some(128000));
        assert_eq!(models[0].max_tokens, Some(4096));
        assert_eq!(models[0].supports_images, Some(true));
        assert_eq!(models[1].id, "b");
        let models =
            parse_models(r#"{"models":[{"slug":"models/gemini","limit":{"context":"1000"}}]}"#)
                .unwrap();
        assert_eq!(models[0].id, "gemini");
        assert_eq!(models[0].context_window, Some(1000));
    }

    #[test]
    fn protocols_use_the_expected_auth_headers() {
        let anthropic = request_headers("anthropic-messages", "secret").unwrap();
        assert_eq!(anthropic["x-api-key"], "secret");
        assert_eq!(anthropic["anthropic-version"], "2023-06-01");
        let openai = request_headers("openai-responses", "secret").unwrap();
        assert_eq!(openai[AUTHORIZATION], "Bearer secret");
    }

    #[test]
    fn rejects_non_http_urls_and_invalid_responses() {
        assert!(model_url_candidates("file:///tmp/models").is_err());
        assert!(parse_models("{}").is_err());
    }
}
