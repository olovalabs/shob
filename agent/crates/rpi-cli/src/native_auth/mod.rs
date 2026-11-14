//! Native provider sign-in and request-time renewal using the shared auth.json.
//! OAuth client identifiers are public application identifiers, not user secrets.
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use rpi_runtime::config::Credential;
use rpi_runtime::credentials::{oauth_needs_refresh, OAuthRefresher, RefreshedOAuth};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

mod custom;
mod device;

/// Select a credential acquisition flow without disabling any other method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMethod {
    ApiKey,
    OAuth,
    DeviceCode,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthMethods {
    pub api_key: bool,
    pub oauth: bool,
}

pub fn supports(provider: &str) -> bool {
    matches!(
        provider,
        "anthropic" | "openai-codex" | "google-gemini-cli" | "openrouter" | "stencil" | "zai"
    ) || device::supports(provider)
        || custom::supports(provider)
}

/// Setup prompts which merely collect an API key are deliberately not OAuth.
pub fn auth_methods(provider: &str) -> AuthMethods {
    AuthMethods {
        api_key: !matches!(provider, "google-antigravity" | "openai-codex-device"),
        oauth: supports(provider) || provider == "google-antigravity",
    }
}

pub enum Event {
    BrowserUrl(String),
    Output(String),
}
pub(super) struct Interaction<'a> {
    emit: &'a dyn Fn(Event),
    inputs: &'a mpsc::Receiver<String>,
}
impl Interaction<'_> {
    pub fn browser(&self, url: &str) {
        (self.emit)(Event::BrowserUrl(url.to_owned()));
    }
    pub fn output(&self, text: &str) {
        (self.emit)(Event::Output(text.to_owned()));
    }
    pub fn input(&self, prompt: &str) -> Result<String, String> {
        self.output(prompt);
        self.inputs
            .recv()
            .map(|v| v.trim().to_owned())
            .map_err(|_| "Authentication was cancelled".into())
    }
    pub fn cancelled(&self) -> Result<(), String> {
        match self.inputs.try_recv() {
            Err(mpsc::TryRecvError::Disconnected) => Err("Authentication was cancelled".into()),
            _ => Ok(()),
        }
    }
    pub async fn pause(&self, seconds: u64) -> Result<(), String> {
        for _ in 0..seconds.saturating_mul(4) {
            self.cancelled()?;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Ok(())
    }
}

// Deliberately not Debug: all fields may include credential material.
pub(super) struct Token {
    pub access: String,
    pub refresh: String,
    pub expires: i64,
    pub extra: Value,
}
pub(super) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
pub(super) fn http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent("kathamo-native-auth/1")
        .build()
        .map_err(|_| "Could not initialize authentication HTTP client".into())
}
pub(super) async fn response_json(response: reqwest::Response) -> Result<Value, String> {
    if !response.status().is_success() {
        return Err(format!(
            "Authentication endpoint returned HTTP {}",
            response.status().as_u16()
        ));
    }
    response
        .json()
        .await
        .map_err(|_| "Authentication endpoint returned invalid JSON".into())
}
pub(super) fn token_response(value: Value, previous: Option<&Token>) -> Result<Token, String> {
    let access = value
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or("Authentication response contained no access token")?
        .to_owned();
    let refresh = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| previous.map(|p| p.refresh.clone()))
        .unwrap_or_default();
    let expires = value
        .get("expires_in")
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.parse().ok()))
        .map(|n| now().saturating_add(n))
        .or_else(|| previous.map(|p| p.expires))
        .unwrap_or(0);
    let mut extra = previous
        .map(|p| p.extra.clone())
        .unwrap_or_else(|| json!({}));
    // Account routing is required by the Codex inference endpoint.
    for key in ["access_token", "id_token"] {
        if let Some(jwt) = value.get(key).and_then(Value::as_str) {
            if let Some(payload) = jwt
                .split('.')
                .nth(1)
                .and_then(|s| URL_SAFE_NO_PAD.decode(s).ok())
                .and_then(|s| serde_json::from_slice::<Value>(&s).ok())
            {
                if let Some(id) = payload
                    .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
                    .and_then(Value::as_str)
                {
                    extra["accountId"] = json!(id);
                }
                if let Some(email) = payload.get("email").and_then(Value::as_str) {
                    extra["email"] = json!(email);
                }
            }
        }
    }
    Ok(Token {
        access,
        refresh,
        expires,
        extra,
    })
}
pub(super) fn pkce() -> (String, String) {
    let verifier = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn parse_callback(
    value: &str,
    expected: &str,
    allow_missing_state: bool,
) -> Result<String, String> {
    if let Ok(url) = reqwest::Url::parse(value) {
        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        if params.contains_key("error") {
            return Err("The provider rejected authorization".into());
        }
        if !allow_missing_state && params.get("state").map(String::as_str) != Some(expected) {
            return Err("OAuth callback state did not match".into());
        }
        return params
            .get("code")
            .filter(|s| !s.is_empty())
            .cloned()
            .ok_or_else(|| "OAuth callback contained no code".into());
    }
    let (code, state) = value.split_once('#').unwrap_or((value, ""));
    if !state.is_empty() && state != expected {
        return Err("OAuth callback state did not match".into());
    }
    if code.trim().is_empty() {
        Err("No authorization code supplied".into())
    } else {
        Ok(code.trim().to_owned())
    }
}

/// Loopback browser callback with a manual redirect/code fallback for remote hosts.
pub(super) async fn browser_code(
    mut authorize: reqwest::Url,
    redirect: &str,
    interaction: &Interaction<'_>,
) -> Result<(String, String, String), String> {
    let (verifier, challenge) = pkce();
    let state = uuid::Uuid::new_v4().to_string();
    let openrouter = authorize.host_str() == Some("openrouter.ai");
    {
        let mut query = authorize.query_pairs_mut();
        if openrouter {
            query.append_pair("callback_url", redirect);
        } else {
            query
                .append_pair("redirect_uri", redirect)
                .append_pair("response_type", "code")
                .append_pair("state", &state);
        }
        query
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
    }
    let redirect_url = reqwest::Url::parse(redirect).map_err(|_| "Invalid OAuth callback URI")?;
    let listener = if matches!(redirect_url.scheme(), "http")
        && matches!(redirect_url.host_str(), Some("localhost" | "127.0.0.1"))
    {
        let port = redirect_url
            .port_or_known_default()
            .ok_or("OAuth callback has no port")?;
        let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|_| {
            format!("OAuth callback port {port} is unavailable; close the other login and retry")
        })?;
        listener
            .set_nonblocking(true)
            .map_err(|_| "Could not initialize OAuth callback listener")?;
        Some(listener)
    } else {
        None
    };
    interaction.browser(authorize.as_str());
    interaction.output("Complete sign-in in your browser. For remote sign-in, paste the final redirect URL or authorization code.");
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        match interaction.inputs.try_recv() {
            Ok(value) => {
                return parse_callback(value.trim(), &state, openrouter)
                    .map(|code| (code, verifier, state))
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err("Authentication was cancelled".into())
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if let Some(listener) = &listener {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut bytes = [0u8; 8192];
                    let size = stream.read(&mut bytes).unwrap_or(0);
                    let request = String::from_utf8_lossy(&bytes[..size]);
                    let target = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("");
                    let callback = reqwest::Url::parse(redirect)
                        .ok()
                        .and_then(|base| base.join(target).ok());
                    if let Some(callback) = callback.filter(|u| u.path() == redirect_url.path()) {
                        let result = parse_callback(callback.as_str(), &state, openrouter);
                        let message = if result.is_ok() {
                            "Sign-in received. You may return to Kathamo."
                        } else {
                            "Sign-in callback was rejected. Return to Kathamo."
                        };
                        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", message.len(), message);
                        return result.map(|code| (code, verifier, state));
                    }
                    let _ = stream.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return Err("Could not receive OAuth callback".into()),
            }
        }
        if Instant::now() >= deadline {
            return Err("Browser sign-in timed out".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn decode_public(value: &str) -> String {
    String::from_utf8(STANDARD.decode(value).expect("public OAuth constant"))
        .expect("public OAuth UTF-8")
}
struct BrowserProvider {
    client: String,
    secret: Option<String>,
    authorize: String,
    endpoint: String,
    redirect: String,
    scope: &'static str,
    json: bool,
}
fn browser_provider(provider: &str) -> Result<BrowserProvider, String> {
    let (client, secret, authorize, endpoint, redirect, scope, json) = match provider {
        "anthropic" => (decode_public("OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl"), None, "https://claude.ai/oauth/authorize".into(), "https://api.anthropic.com/v1/oauth/token".into(), "http://localhost:54545/callback".into(), "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload", true),
        "openai-codex" => ("app_EMoamEEZ73f0CkXaXp7hrann".into(), None, "https://auth.openai.com/oauth/authorize".into(), "https://auth.openai.com/oauth/token".into(), "http://localhost:1455/auth/callback".into(), "openid profile email offline_access api.connectors.read api.connectors.invoke", false),
        "google-gemini-cli" => {
            let client = std::env::var("KATHAMO_GEMINI_CLI_CLIENT_ID")
                .ok().filter(|value| !value.trim().is_empty())
                .ok_or("Configure KATHAMO_GEMINI_CLI_CLIENT_ID for Gemini CLI OAuth")?;
            let secret = std::env::var("KATHAMO_GEMINI_CLI_CLIENT_SECRET")
                .ok().filter(|value| !value.trim().is_empty())
                .ok_or("Configure KATHAMO_GEMINI_CLI_CLIENT_SECRET for Gemini CLI OAuth")?;
            (client, Some(secret), "https://accounts.google.com/o/oauth2/v2/auth".into(), "https://oauth2.googleapis.com/token".into(), "http://127.0.0.1:8085/oauth2callback".into(), "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile", false)
        },
        "stencil" => { let auth = std::env::var("STENCIL_AUTH_URL").unwrap_or_else(|_| "https://auth.stencil.so".into()); (std::env::var("STENCIL_CLIENT_ID").unwrap_or_else(|_| "omp".into()), None, format!("{}/oauth/authorize", auth.trim_end_matches('/')), format!("{}/oauth/token", auth.trim_end_matches('/')), std::env::var("STENCIL_REDIRECT_URI").unwrap_or_else(|_| "http://127.0.0.1:54547/callback".into()), "openid profile email offline_access", false) },
        _ => return Err("This provider has no native browser OAuth flow".into()),
    };
    Ok(BrowserProvider {
        client,
        secret,
        authorize,
        endpoint,
        redirect,
        scope,
        json,
    })
}
async fn browser_login(provider: &str, interaction: &Interaction<'_>) -> Result<Token, String> {
    if provider == "openrouter" {
        let (code, verifier, _) = browser_code(
            reqwest::Url::parse("https://openrouter.ai/auth").unwrap(),
            "http://localhost:54549/callback",
            interaction,
        )
        .await?;
        let raw = response_json(
            http()?
                .post("https://openrouter.ai/api/v1/auth/keys")
                .json(&json!({"code":code,"code_verifier":verifier,"code_challenge_method":"S256"}))
                .send()
                .await
                .map_err(|_| "OpenRouter authorization exchange failed")?,
        )
        .await?;
        return Ok(Token {
            access: raw
                .get("key")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or("OpenRouter returned no API key")?
                .into(),
            refresh: String::new(),
            expires: 0,
            extra: json!({}),
        });
    }
    let config = browser_provider(provider)?;
    let mut authorize =
        reqwest::Url::parse(&config.authorize).map_err(|_| "Invalid authorization URL")?;
    authorize
        .query_pairs_mut()
        .append_pair("client_id", &config.client)
        .append_pair("scope", config.scope);
    match provider {
        "anthropic" => {
            authorize.query_pairs_mut().append_pair("code", "true");
        }
        "openai-codex" => {
            authorize
                .query_pairs_mut()
                .append_pair("id_token_add_organizations", "true")
                .append_pair("codex_cli_simplified_flow", "true")
                .append_pair("originator", "omp");
        }
        "google-gemini-cli" => {
            authorize
                .query_pairs_mut()
                .append_pair("access_type", "offline")
                .append_pair("prompt", "consent");
        }
        _ => {}
    }
    let (code, verifier, state) = browser_code(authorize, &config.redirect, interaction).await?;
    let mut fields = json!({"grant_type":"authorization_code", "client_id":config.client,"code":code,"redirect_uri":config.redirect,"code_verifier":verifier});
    if let Some(secret) = config.secret {
        fields["client_secret"] = json!(secret);
    }
    if provider == "anthropic" {
        fields["state"] = json!(state);
    }
    let request = http()?.post(&config.endpoint);
    let request = if config.json {
        request.json(&fields)
    } else {
        request.form(&fields)
    };
    let mut token = token_response(
        response_json(
            request
                .send()
                .await
                .map_err(|_| "OAuth authorization exchange failed")?,
        )
        .await?,
        None,
    )?;
    if provider == "google-gemini-cli" {
        if token.refresh.is_empty() {
            return Err(
                "Google returned no refresh token; revoke the previous grant and sign in again"
                    .into(),
            );
        }
        token.extra["projectId"] = json!(gemini_project(&token.access, interaction).await?);
    }
    Ok(token)
}
async fn browser_refresh(provider: &str, old: &Token) -> Result<Token, String> {
    if provider == "openrouter" {
        return Ok(Token {
            access: old.access.clone(),
            refresh: old.refresh.clone(),
            expires: 0,
            extra: old.extra.clone(),
        });
    }
    let config = browser_provider(provider)?;
    let mut fields =
        json!({"grant_type":"refresh_token","client_id":config.client,"refresh_token":old.refresh});
    if let Some(secret) = config.secret {
        fields["client_secret"] = json!(secret);
    }
    let mut request = http()?.post(&config.endpoint);
    if provider == "anthropic" {
        request = request.header("anthropic-beta", "oauth-2025-04-20");
    }
    let request = if config.json {
        request.json(&fields)
    } else {
        request.form(&fields)
    };
    token_response(
        response_json(
            request
                .send()
                .await
                .map_err(|_| "OAuth token renewal failed")?,
        )
        .await?,
        Some(old),
    )
}

async fn gemini_project(access: &str, interaction: &Interaction<'_>) -> Result<String, String> {
    let client = http()?;
    let env = std::env::var("GOOGLE_CLOUD_PROJECT")
        .or_else(|_| std::env::var("GOOGLE_CLOUD_PROJECT_ID"))
        .ok()
        .filter(|s| !s.is_empty());
    let metadata = json!({"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI","duetProject":env});
    let endpoint = "https://cloudcode-pa.googleapis.com/v1internal";
    interaction.output("Checking your Cloud Code Assist project...");
    let response = client
        .post(format!("{endpoint}:loadCodeAssist"))
        .bearer_auth(access)
        .header("User-Agent", "GeminiCLI/0.43.0")
        .json(&json!({"cloudaicompanionProject":env,"metadata":metadata}))
        .send()
        .await
        .map_err(|_| "Cloud Code Assist project discovery failed")?;
    let status = response.status();
    let mut data: Value = response
        .json()
        .await
        .map_err(|_| "Cloud Code Assist returned invalid JSON")?;
    if !status.is_success() {
        let vpc = data
            .pointer("/error/details")
            .and_then(Value::as_array)
            .is_some_and(|ds| {
                ds.iter().any(|d| {
                    d.get("reason").and_then(Value::as_str) == Some("SECURITY_POLICY_VIOLATED")
                })
            });
        if vpc {
            data = json!({"currentTier":{"id":"standard-tier"}});
        } else {
            return Err(format!(
                "Cloud Code Assist project discovery returned HTTP {}",
                status.as_u16()
            ));
        }
    }
    if data.get("currentTier").is_some_and(|v| !v.is_null()) {
        return data
            .get("cloudaicompanionProject")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(env)
            .ok_or_else(|| {
                "Set GOOGLE_CLOUD_PROJECT for this Cloud Code Assist account, then sign in again"
                    .into()
            });
    }
    let tier = data
        .get("allowedTiers")
        .and_then(Value::as_array)
        .and_then(|tiers| {
            tiers
                .iter()
                .find(|tier| tier.get("isDefault").and_then(Value::as_bool) == Some(true))
        })
        .and_then(|tier| tier.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("free-tier");
    if tier != "free-tier" && env.is_none() {
        return Err(
            "Set GOOGLE_CLOUD_PROJECT for this Cloud Code Assist account, then sign in again"
                .into(),
        );
    }
    let mut body = json!({"tierId":tier,"metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}});
    if tier != "free-tier" {
        body["cloudaicompanionProject"] = json!(env);
        body["metadata"]["duetProject"] = json!(env);
    }
    interaction.output("Provisioning your Cloud Code Assist project...");
    let mut operation = response_json(
        client
            .post(format!("{endpoint}:onboardUser"))
            .bearer_auth(access)
            .header("User-Agent", "GeminiCLI/0.43.0")
            .json(&body)
            .send()
            .await
            .map_err(|_| "Cloud Code Assist provisioning failed")?,
    )
    .await?;
    for _ in 0..24 {
        if operation.get("done").and_then(Value::as_bool) == Some(true) {
            break;
        }
        let name = operation
            .get("name")
            .and_then(Value::as_str)
            .ok_or("Cloud Code Assist returned no provisioning operation")?
            .to_owned();
        interaction.pause(5).await?;
        operation = response_json(
            client
                .get(format!("{endpoint}/{name}"))
                .bearer_auth(access)
                .send()
                .await
                .map_err(|_| "Cloud Code Assist provisioning polling failed")?,
        )
        .await?;
    }
    if operation.get("done").and_then(Value::as_bool) != Some(true) {
        return Err("Cloud Code Assist project provisioning timed out".into());
    }
    operation
        .pointer("/response/cloudaicompanionProject/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or(env)
        .ok_or_else(|| {
            "Cloud Code Assist returned no project; set GOOGLE_CLOUD_PROJECT and sign in again"
                .into()
        })
}

fn storage_provider(provider: &str) -> &str {
    match provider {
        "openai-codex-device" => "openai-codex",
        "devin-cli" | "devin-windsurf" => "devin",
        "zai-coding-plan" => "zai",
        _ => provider,
    }
}
fn inference_access(provider: &str, token: &Token) -> String {
    if !matches!(
        provider,
        "openai-codex"
            | "github-copilot"
            | "google-gemini-cli"
            | "google-antigravity"
            | "devin"
            | "cursor"
            | "alibaba-coding-plan"
            | "snowflake"
    ) || token.extra.as_object().is_none_or(|m| m.is_empty())
    {
        return token.access.clone();
    }
    let mut value = json!({});
    for key in [
        "apiEndpoint",
        "enterpriseUrl",
        "devinApiUrl",
        "projectId",
        "accountId",
        "email",
        "account",
        "host",
        "baseUrl",
        "orgId",
        "activeOrganizationId",
        "region",
        "inferenceRegion",
        "sessionId",
    ] {
        if let Some(metadata) = token.extra.get(key) {
            value[key] = metadata.clone();
        }
    }
    if provider == "snowflake" && value.get("account").is_none() {
        if let Some(account) = token.extra.get("apiEndpoint") {
            value["account"] = account.clone();
        }
    }
    value["token"] = json!(token.access);
    value.to_string()
}
fn stored_refresh(provider: &str, token: &Token) -> String {
    json!({"native_auth":1,"provider":provider,"refresh":token.refresh,"extra":token.extra})
        .to_string()
}
fn unpack(access: &str, refresh: &str, expires: i64) -> (String, Token) {
    let wrapper: Value = serde_json::from_str(refresh).unwrap_or(Value::Null);
    let structured: Value = serde_json::from_str(access).unwrap_or(Value::Null);
    let token = Token {
        access: structured
            .get("token")
            .and_then(Value::as_str)
            .unwrap_or(access)
            .to_owned(),
        refresh: wrapper
            .get("refresh")
            .and_then(Value::as_str)
            .unwrap_or(refresh)
            .to_owned(),
        expires,
        extra: wrapper.get("extra").cloned().unwrap_or_else(|| json!({})),
    };
    (
        wrapper
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        token,
    )
}

pub fn login(
    provider: &str,
    emit: &dyn Fn(Event),
    inputs: &mpsc::Receiver<String>,
    device_code: bool,
) -> Result<(), String> {
    let flow = match provider {
        "openai-codex" if device_code => "openai-codex-device",
        "zai" => "zai-coding-plan",
        _ => provider,
    };
    if !supports(flow) {
        return Err("This provider has no native OAuth sign-in; use its API-key method".into());
    }
    let interaction = Interaction { emit, inputs };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "Could not initialize native authentication")?;
    let token = runtime.block_on(async {
        if device::supports(flow) {
            device::login(flow, &interaction).await
        } else if custom::supports(flow) {
            custom::login(flow, &interaction).await
        } else {
            browser_login(flow, &interaction).await
        }
    })?;
    interaction.cancelled()?;
    let destination = storage_provider(flow);
    if flow == "openrouter" {
        crate::config::upsert_credential(
            destination,
            Credential::ApiKey {
                key: Some(token.access),
                env: None,
            },
        )
    } else {
        crate::config::upsert_oauth_token(
            destination,
            inference_access(destination, &token),
            stored_refresh(flow, &token),
            token.expires,
        )
    }
    .map_err(|_| "Could not persist authentication credentials".into())
}

/// Combines native renewal with extension-owned renewal; extensions retain priority.
pub fn refresher(extension: Option<Arc<dyn OAuthRefresher>>) -> Arc<dyn OAuthRefresher> {
    Arc::new(NativeRefresher { extension })
}
struct NativeRefresher {
    extension: Option<Arc<dyn OAuthRefresher>>,
}
impl OAuthRefresher for NativeRefresher {
    fn supports(&self, provider: &str) -> bool {
        self.extension
            .as_ref()
            .is_some_and(|e| e.supports(provider))
            || matches!(
                provider,
                "anthropic"
                    | "openai-codex"
                    | "openai-codex-device"
                    | "google-gemini-cli"
                    | "github-copilot"
                    | "gitlab-duo"
                    | "gitlab-duo-agent"
                    | "kimi-code"
                    | "cursor"
                    | "xai-oauth"
                    | "factory-droid"
                    | "snowflake"
                    | "stencil"
            )
    }
    fn refresh(&self, provider: &str) -> Result<RefreshedOAuth, String> {
        if let Some(extension) = self.extension.as_ref().filter(|e| e.supports(provider)) {
            return extension.refresh(provider);
        }
        // The request binding calls this on a blocking worker. A dedicated thread
        // also makes manual refresh safe when invoked from a Tokio executor.
        let provider = provider.to_owned();
        std::thread::spawn(move || refresh_native(&provider))
            .join()
            .map_err(|_| "Native OAuth renewal worker failed".to_owned())?
    }
}
fn refresh_native(provider: &str) -> Result<RefreshedOAuth, String> {
    static LOCKS: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<String, Arc<std::sync::Mutex<()>>>>,
    > = std::sync::LazyLock::new(Default::default);
    let lock = LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(provider.to_owned())
        .or_default()
        .clone();
    let _guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(Credential::Oauth {
        access,
        refresh,
        expires,
    }) = crate::config::read_oauth_token(provider)
        .map_err(|_| "Could not read stored OAuth credentials")?
    else {
        return Err("No stored OAuth credential; sign in again".into());
    };
    if !oauth_needs_refresh(expires, now()) {
        return Ok(RefreshedOAuth { access, expires });
    }
    let (flow, old) = unpack(&access, &refresh, expires);
    let flow = if flow.is_empty() { provider } else { &flow };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "Could not initialize OAuth renewal")?;
    let token = runtime.block_on(async {
        if device::supports(flow) {
            device::refresh(flow, &old).await
        } else if custom::supports(flow) {
            custom::refresh(flow, &old).await
        } else {
            browser_refresh(flow, &old).await
        }
    })?;
    if token.access.trim().is_empty() || (token.expires != 0 && token.expires <= now()) {
        return Err("OAuth renewal returned an invalid or expired token; sign in again".into());
    }
    let renewed_access = inference_access(provider, &token);
    let committed = crate::config::replace_oauth_token_if_unchanged(
        provider,
        (&access, &refresh, expires),
        renewed_access.clone(),
        stored_refresh(flow, &token),
        token.expires,
    )
    .map_err(|_| "Could not save renewed OAuth credentials")?;
    if !committed {
        return match crate::config::read_oauth_token(provider)
            .map_err(|_| "Could not read current credentials")?
        {
            Some(Credential::Oauth {
                access, expires, ..
            }) => Ok(RefreshedOAuth { access, expires }),
            _ => Err("Authentication changed during renewal".into()),
        };
    }
    Ok(RefreshedOAuth {
        access: renewed_access,
        expires: token.expires,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_setup_is_not_oauth() {
        for provider in [
            "qwen-portal",
            "alibaba-coding-plan",
            "alibaba-token-plan",
            "minimax-code",
            "cloudflare-workers-ai",
            "unknown-provider",
        ] {
            assert_eq!(
                auth_methods(provider),
                AuthMethods {
                    api_key: true,
                    oauth: false
                }
            );
        }
        for provider in [
            "anthropic",
            "openrouter",
            "mistral",
            "kimi-code",
            "github-copilot",
            "gitlab-duo",
            "cursor",
            "zai",
        ] {
            assert_eq!(
                auth_methods(provider),
                AuthMethods {
                    api_key: true,
                    oauth: true
                }
            );
        }
    }

    #[test]
    fn callback_checks_state_without_echoing_codes() {
        assert_eq!(
            parse_callback(
                "http://localhost/callback?code=secret&state=expected",
                "expected",
                false
            )
            .unwrap(),
            "secret"
        );
        assert!(parse_callback(
            "http://localhost/callback?code=secret&state=wrong",
            "expected",
            false
        )
        .is_err());
        assert!(
            parse_callback("http://localhost/callback?code=secret", "expected", false).is_err()
        );
        assert_eq!(
            parse_callback("code#expected", "expected", false).unwrap(),
            "code"
        );
        assert!(parse_callback("code#wrong", "expected", false).is_err());
        assert_eq!(
            parse_callback("http://localhost/callback?code=key", "unused", true).unwrap(),
            "key"
        );
    }

    #[test]
    fn native_metadata_round_trips_without_refresh_secrets_in_inference() {
        let token = Token {
            access: "access".into(),
            refresh: "refresh".into(),
            expires: 123,
            extra: json!({"projectId":"project","oauthAccessToken":"private"}),
        };
        let access = inference_access("google-gemini-cli", &token);
        assert!(!access.contains("private"));
        assert!(!access.contains("refresh"));
        let (flow, decoded) = unpack(&access, &stored_refresh("google-gemini-cli", &token), 123);
        assert_eq!(flow, "google-gemini-cli");
        assert_eq!(decoded.access, "access");
        assert_eq!(decoded.refresh, "refresh");
        assert_eq!(decoded.extra["projectId"], "project");
        assert_eq!(inference_access("mistral", &token), "access");
    }

    #[test]
    fn snowflake_inference_preserves_validated_account_without_refresh_secret() {
        let token = Token {
            access: "access".into(),
            refresh: "refresh".into(),
            expires: 123,
            extra: json!({"apiEndpoint":"https://account.snowflakecomputing.com","enterpriseUrl":"https://account.snowflakecomputing.com"}),
        };
        let access: Value = serde_json::from_str(&inference_access("snowflake", &token)).unwrap();
        assert_eq!(access["token"], "access");
        assert_eq!(access["account"], "https://account.snowflakecomputing.com");
        assert_eq!(
            access["apiEndpoint"],
            "https://account.snowflakecomputing.com"
        );
        assert!(access.get("refresh").is_none());
    }

    #[test]
    fn renewal_preserves_nonrotated_refresh_and_routing() {
        let old = Token {
            access: "old".into(),
            refresh: "refresh".into(),
            expires: 1,
            extra: json!({"projectId":"project"}),
        };
        let renewed =
            token_response(json!({"access_token":"new","expires_in":3600}), Some(&old)).unwrap();
        assert_eq!(renewed.refresh, "refresh");
        assert_eq!(renewed.extra["projectId"], "project");
        assert!(renewed.expires > now());
        assert!(!refresher(None).supports("devin"));
        assert!(refresher(None).supports("github-copilot"));
    }
}
