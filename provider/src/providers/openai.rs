use crate::auth::api_key::build_auth_headers;
use crate::catalog::get_provider;
use crate::types::ProviderSpec;
use std::collections::HashMap;

pub const OPENAI_PROVIDER_ID: &str = "openai";
pub const OPENAI_CODEX_PROVIDER_ID: &str = "openai-codex";
pub const CHAT_COMPLETIONS_URL: &str = "https://api.openai.com/v1/chat/completions";
pub const RESPONSES_URL: &str = "https://api.openai.com/v1/responses";

/// Retrieve the OpenAI provider specification.
pub fn spec() -> &'static ProviderSpec {
    get_provider(OPENAI_PROVIDER_ID).expect("openai provider must exist in catalog")
}

/// Retrieve the OpenAI Codex provider specification.
pub fn codex_spec() -> &'static ProviderSpec {
    get_provider(OPENAI_CODEX_PROVIDER_ID).expect("openai-codex provider must exist in catalog")
}

/// Build headers for an OpenAI API request.
pub fn headers(api_key: &str) -> HashMap<String, String> {
    build_auth_headers(spec(), api_key)
}
