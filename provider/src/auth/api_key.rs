use crate::types::ProviderSpec;
use std::collections::HashMap;

/// Result of checking API key availability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiKeyResolution {
    pub key: String,
    pub source: String,
}

/// Look up an API key for a given provider specification.
pub fn resolve_api_key(spec: &ProviderSpec) -> Option<ApiKeyResolution> {
    // 1. Check all declared environment variables
    for var in &spec.env_vars {
        if let Ok(val) = std::env::var(var) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(ApiKeyResolution {
                    key: trimmed.to_string(),
                    source: format!("Environment ({var})"),
                });
            }
        }
    }

    // 2. Check conventional fallback names
    let fallback_var = match spec.id.as_str() {
        "openai" | "openai-codex" => Some("OPENAI_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "google" | "google-antigravity" | "google-gemini-cli" => Some("GEMINI_API_KEY"),
        "deepseek" => Some("DEEPSEEK_API_KEY"),
        "groq" => Some("GROQ_API_KEY"),
        "xai" | "xai-oauth" => Some("XAI_API_KEY"),
        "cerebras" => Some("CEREBRAS_API_KEY"),
        "mistral" => Some("MISTRAL_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
        "together" => Some("TOGETHER_API_KEY"),
        "perplexity" => Some("PERPLEXITY_API_KEY"),
        "cohere" => Some("COHERE_API_KEY"),
        "github-copilot" => Some("COPILOT_GITHUB_TOKEN"),
        _ => None,
    };

    if let Some(var) = fallback_var {
        if let Ok(val) = std::env::var(var) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Some(ApiKeyResolution {
                    key: trimmed.to_string(),
                    source: format!("Environment ({var})"),
                });
            }
        }
    }

    None
}

/// Construct appropriate authorization HTTP headers for a given provider and API key.
pub fn build_auth_headers(spec: &ProviderSpec, key: &str) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    match spec.api_key_format.as_str() {
        "bearer" => {
            headers.insert("Authorization".to_string(), format!("Bearer {key}"));
        }
        "basic" => {
            headers.insert("Authorization".to_string(), format!("Basic {key}"));
        }
        _ => {
            headers.insert("Authorization".to_string(), format!("Bearer {key}"));
        }
    }

    // Provider-specific headers
    match spec.id.as_str() {
        "anthropic" => {
            headers.insert("x-api-key".to_string(), key.to_string());
            headers.insert("anthropic-version".to_string(), "2023-06-01".to_string());
        }
        "azure" => {
            headers.insert("api-key".to_string(), key.to_string());
        }
        "google" | "google-antigravity" => {
            headers.insert("x-goog-api-key".to_string(), key.to_string());
        }
        _ => {}
    }

    headers
}
