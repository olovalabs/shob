use crate::auth::callback::CallbackServer;
use crate::auth::pkce::{PkceChallenge, generate_pkce, generate_state};
use crate::auth::storage::save_local_credential;
use crate::types::{ProviderSpec, StoredCredential};
use serde::{Deserialize, Serialize};
use std::process::Command;
use std::time::Duration;

/// Result of a successful OAuth authorization code exchange.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OAuthTokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
}

/// Construct the complete browser authorization URL for an OAuth provider.
pub fn build_authorization_url(
    spec: &ProviderSpec,
    redirect_uri: &str,
    state: &str,
    pkce: Option<&PkceChallenge>,
) -> Result<String, String> {
    let login = spec
        .login
        .as_ref()
        .ok_or_else(|| format!("Provider {} has no OAuth login configuration", spec.id))?;

    let auth_base = login
        .authorize_url
        .as_deref()
        .ok_or_else(|| format!("Provider {} has no authorize_url configured", spec.id))?;

    let client_id = login
        .client_id
        .as_deref()
        .ok_or_else(|| format!("Provider {} has no client_id configured", spec.id))?;

    let sep = login.scope_separator.as_deref().unwrap_or(" ");
    let scope_str = login.scopes.join(sep);

    let mut query_params = Vec::new();
    query_params.push(format!("response_type=code"));
    query_params.push(format!("client_id={}", urlencoding_encode(client_id)));
    query_params.push(format!("redirect_uri={}", urlencoding_encode(redirect_uri)));

    if !scope_str.is_empty() {
        query_params.push(format!("scope={}", urlencoding_encode(&scope_str)));
    }

    query_params.push(format!("state={}", urlencoding_encode(state)));

    if let Some(pkce) = pkce {
        query_params.push(format!(
            "code_challenge={}",
            urlencoding_encode(&pkce.challenge)
        ));
        query_params.push(format!("code_challenge_method={}", pkce.method));
    }

    // Provider-specific authorization parameters
    match spec.id.as_str() {
        "openai-codex" => {
            query_params.push("id_token_add_organizations=true".to_string());
            query_params.push("codex_cli_simplified_flow=true".to_string());
            query_params.push("originator=omp".to_string());
        }
        "anthropic" => {
            query_params.push("access_type=offline".to_string());
            query_params.push("prompt=consent".to_string());
        }
        "google-antigravity" | "google-gemini-cli" => {
            query_params.push("access_type=offline".to_string());
            query_params.push("prompt=consent".to_string());
        }
        _ => {}
    }

    let delimiter = if auth_base.contains('?') { '&' } else { '?' };
    Ok(format!("{auth_base}{delimiter}{}", query_params.join("&")))
}

/// Exchange an authorization code for tokens using curl.
pub fn exchange_auth_code(
    spec: &ProviderSpec,
    code: &str,
    redirect_uri: &str,
    pkce_verifier: Option<&str>,
) -> Result<OAuthTokenResponse, String> {
    let login = spec
        .login
        .as_ref()
        .ok_or_else(|| format!("Provider {} has no login configuration", spec.id))?;

    let token_url = login
        .token_url
        .as_deref()
        .ok_or_else(|| format!("Provider {} has no token_url configured", spec.id))?;

    let client_id = login
        .client_id
        .as_deref()
        .ok_or_else(|| format!("Provider {} has no client_id configured", spec.id))?;

    let mut body_params = Vec::new();
    body_params.push(format!("grant_type=authorization_code"));
    body_params.push(format!("code={}", urlencoding_encode(code)));
    body_params.push(format!("redirect_uri={}", urlencoding_encode(redirect_uri)));
    body_params.push(format!("client_id={}", urlencoding_encode(client_id)));

    if let Some(verifier) = pkce_verifier {
        body_params.push(format!("code_verifier={}", urlencoding_encode(verifier)));
    }

    let body = body_params.join("&");

    let output = Command::new("curl")
        .args(["-s", "-X", "POST", token_url])
        .args(["-H", "Accept: application/json"])
        .args(["-H", "Content-Type: application/x-www-form-urlencoded"])
        .args(["-d", &body])
        .output()
        .map_err(|e| format!("Failed to execute token exchange request: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "OAuth token exchange request failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let token_resp: OAuthTokenResponse = serde_json::from_slice(&output.stdout).map_err(|e| {
        format!(
            "Invalid token response from {token_url}: {e}. Response body: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })?;

    Ok(token_resp)
}

/// Refresh an expired OAuth access token.
pub fn refresh_oauth_token(
    spec: &ProviderSpec,
    refresh_token: &str,
) -> Result<OAuthTokenResponse, String> {
    let token_url = spec
        .refresh
        .as_ref()
        .and_then(|r| r.token_url.as_deref())
        .or_else(|| spec.login.as_ref().and_then(|l| l.token_url.as_deref()))
        .ok_or_else(|| format!("Provider {} has no token URL for refresh", spec.id))?;

    let client_id = spec
        .login
        .as_ref()
        .and_then(|l| l.client_id.as_deref())
        .ok_or_else(|| format!("Provider {} has no client_id for refresh", spec.id))?;

    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        urlencoding_encode(refresh_token),
        urlencoding_encode(client_id)
    );

    let output = Command::new("curl")
        .args(["-s", "-X", "POST", token_url])
        .args(["-H", "Accept: application/json"])
        .args(["-H", "Content-Type: application/x-www-form-urlencoded"])
        .args(["-d", &body])
        .output()
        .map_err(|e| format!("Failed to execute token refresh request: {e}"))?;

    let token_resp: OAuthTokenResponse = serde_json::from_slice(&output.stdout).map_err(|e| {
        format!(
            "Invalid token refresh response from {token_url}: {e}. Output: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })?;

    Ok(token_resp)
}

/// Run an interactive OAuth login for a provider, using a local callback server.
pub fn run_interactive_oauth(
    spec: &ProviderSpec,
    on_auth_url: impl Fn(&str, &str),
) -> Result<StoredCredential, String> {
    let pkce = if spec.login.as_ref().and_then(|l| l.pkce).unwrap_or(true) {
        Some(generate_pkce())
    } else {
        None
    };

    let state = generate_state();
    let callback_server = CallbackServer::bind(spec.callback_port)?;
    let redirect_uri = callback_server.redirect_uri("/callback");

    let auth_url = build_authorization_url(spec, &redirect_uri, &state, pkce.as_ref())?;

    let instructions = spec
        .login
        .as_ref()
        .and_then(|l| l.instructions.as_deref())
        .unwrap_or("Complete login in your browser, then return to Kathamo.");

    on_auth_url(&auth_url, instructions);

    // Try to open system browser automatically
    let _ = open_browser(&auth_url);

    // Wait for the browser redirect callback
    let callback = callback_server.wait_for_callback(Duration::from_secs(300))?;

    if let Some(err) = callback.error {
        let desc = callback.error_description.unwrap_or(err);
        return Err(format!("Authorization failed: {desc}"));
    }

    let code = callback
        .code
        .ok_or_else(|| "No authorization code returned from OAuth callback".to_string())?;

    // Exchange authorization code for token
    let tokens = exchange_auth_code(
        spec,
        &code,
        &redirect_uri,
        pkce.as_ref().map(|p| p.verifier.as_str()),
    )?;

    let expires = tokens.expires_in.map(|s| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + s
    });

    let cred = StoredCredential::OAuth {
        access: tokens.access_token,
        refresh: tokens.refresh_token,
        expires,
        email: None,
        account_id: None,
        org_id: None,
        org_name: None,
        authorized_at: Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        ),
    };

    let _ = save_local_credential(&spec.id, cred.clone());
    Ok(cred)
}

fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(target_os = "windows")]
    {
        Command::new("cmd")
            .args(["/c", "start", url])
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

fn urlencoding_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(b as char);
            }
            _ => {
                result.push_str(&format!("%{b:02X}"));
            }
        }
    }
    result
}
