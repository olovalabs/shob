use crate::auth::api_key::build_auth_headers;
use crate::catalog::get_provider;
use crate::types::ProviderSpec;
use std::collections::HashMap;

pub const ANTHROPIC_PROVIDER_ID: &str = "anthropic";
pub const BOOTSTRAP_URL: &str = "https://api.anthropic.com/api/claude_cli/bootstrap";
pub const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";

/// Anthropic identity metadata retrieved from bootstrap endpoint.
#[derive(Clone, Debug, Default)]
pub struct AnthropicIdentity {
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub org_id: Option<String>,
    pub org_name: Option<String>,
}

/// Retrieve the Anthropic provider specification.
pub fn spec() -> &'static ProviderSpec {
    get_provider(ANTHROPIC_PROVIDER_ID).expect("anthropic provider must exist in catalog")
}

/// Build headers for an Anthropic API request.
pub fn headers(api_key: &str) -> HashMap<String, String> {
    build_auth_headers(spec(), api_key)
}
