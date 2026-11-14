use crate::catalog::get_provider;
use crate::types::ProviderSpec;

pub const GITHUB_COPILOT_PROVIDER_ID: &str = "github-copilot";
pub const OPENCODE_CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
pub const COPILOT_DEVICE_AUTH_URL: &str = "https://github.com/login/device/code";
pub const COPILOT_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

pub fn spec() -> &'static ProviderSpec {
    get_provider(GITHUB_COPILOT_PROVIDER_ID).expect("github-copilot provider must exist in catalog")
}
