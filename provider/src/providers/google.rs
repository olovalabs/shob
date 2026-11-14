use crate::auth::api_key::build_auth_headers;
use crate::catalog::get_provider;
use crate::types::ProviderSpec;
use std::collections::HashMap;

pub const GOOGLE_PROVIDER_ID: &str = "google";
pub const GOOGLE_ANTIGRAVITY_PROVIDER_ID: &str = "google-antigravity";
pub const GOOGLE_GEMINI_CLI_PROVIDER_ID: &str = "google-gemini-cli";
pub const CLOUD_CODE_ASSIST_ENDPOINT: &str = "https://daily-cloudcode-pa.googleapis.com";

pub fn spec() -> &'static ProviderSpec {
    get_provider(GOOGLE_PROVIDER_ID).expect("google provider must exist in catalog")
}

pub fn antigravity_spec() -> &'static ProviderSpec {
    get_provider(GOOGLE_ANTIGRAVITY_PROVIDER_ID)
        .expect("google-antigravity provider must exist in catalog")
}

pub fn gemini_cli_spec() -> &'static ProviderSpec {
    get_provider(GOOGLE_GEMINI_CLI_PROVIDER_ID)
        .expect("google-gemini-cli provider must exist in catalog")
}

pub fn headers(api_key: &str) -> HashMap<String, String> {
    build_auth_headers(spec(), api_key)
}
