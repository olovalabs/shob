use std::time::Duration;

use base64::Engine;
use reqwest::{RequestBuilder, Response, Url};
use serde_json::{json, Value};
use tokio::time::Instant;

use super::{browser_code, http, now, response_json, token_response, Interaction, Token};

const CODEX_CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_TOKEN: &str = "https://auth.openai.com/oauth/token";
const KIMI_CLIENT: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const GITLAB_CLIENT: &str = "da4edff2e6ebd2bc3208611e2768bc1c1dd7be791dc5ff26ca34ca9ee44f7d4b";
const GITLAB_AGENT_CLIENT: &str =
    "36f2a70cddeb5a0889d4fd8295c241b7e9848e89cf9e599d0eed2d8e5350fbf5";

pub(super) fn supports(provider: &str) -> bool {
    matches!(
        provider,
        "github-copilot" | "openai-codex-device" | "kimi-code" | "gitlab-duo" | "gitlab-duo-agent"
    )
}

pub(super) async fn login(provider: &str, interaction: &Interaction<'_>) -> Result<Token, String> {
    interaction.cancelled()?;
    match provider {
        "github-copilot" => copilot_login(interaction).await,
        "openai-codex-device" => codex_login(interaction).await,
        "kimi-code" => kimi_login(interaction).await,
        "gitlab-duo" | "gitlab-duo-agent" => gitlab_login(provider, interaction).await,
        _ => Err("This provider does not support native device OAuth".into()),
    }
}

pub(super) async fn refresh(provider: &str, previous: &Token) -> Result<Token, String> {
    if previous.refresh.is_empty() {
        return Err("OAuth refresh credential is missing; sign in again".into());
    }
    match provider {
        "github-copilot" => copilot_exchange(&previous.refresh, previous.extra.clone(), None).await,
        "openai-codex-device" => {
            let raw = request_json(
                http()?.post(CODEX_TOKEN).form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", previous.refresh.as_str()),
                    ("client_id", CODEX_CLIENT),
                ]),
                None,
            )
            .await?;
            codex_token(raw, Some(previous))
        }
        "kimi-code" => {
            let base = previous
                .extra
                .get("oauthHost")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(kimi_base);
            let device = previous
                .extra
                .get("deviceId")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(new_device_id);
            let raw = request_json(
                kimi_request(http()?.post(format!("{base}/api/oauth/token")), &device).form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", previous.refresh.as_str()),
                    ("client_id", KIMI_CLIENT),
                ]),
                None,
            )
            .await?;
            kimi_token(raw, Some(previous), &base, &device)
        }
        "gitlab-duo" | "gitlab-duo-agent" => {
            let (default_client, default_redirect) = gitlab_settings(provider);
            let client = previous
                .extra
                .get("clientId")
                .and_then(Value::as_str)
                .unwrap_or(&default_client);
            let redirect = previous
                .extra
                .get("redirectUri")
                .and_then(Value::as_str)
                .unwrap_or(&default_redirect);
            let raw = request_json(
                http()?.post("https://gitlab.com/oauth/token").form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", previous.refresh.as_str()),
                    ("client_id", client),
                    ("redirect_uri", redirect),
                ]),
                None,
            )
            .await?;
            gitlab_token(raw, Some(previous), client, redirect)
        }
        _ => Err("This provider does not support native device OAuth refresh".into()),
    }
}

fn field<'a>(raw: &'a Value, name: &str) -> Result<&'a str, String> {
    raw.get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("OAuth response is missing {name}"))
}

fn seconds(value: Option<&Value>) -> Option<u64> {
    value.and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
}

// Check cancellation during network waits as well as between bounded poll attempts.
async fn send(
    request: RequestBuilder,
    interaction: Option<&Interaction<'_>>,
) -> Result<Response, String> {
    let request = request.timeout(Duration::from_secs(15)).send();
    tokio::pin!(request);
    loop {
        if let Some(interaction) = interaction {
            interaction.cancelled()?;
        }
        tokio::select! {
            response = &mut request => return response.map_err(|_| "OAuth request failed".into()),
            _ = tokio::time::sleep(Duration::from_millis(250)), if interaction.is_some() => {}
        }
    }
}

async fn request_json(
    request: RequestBuilder,
    interaction: Option<&Interaction<'_>>,
) -> Result<Value, String> {
    let response = send(request, interaction).await?;
    response_json(response).await
}

async fn poll_json(response: Response) -> Result<Value, String> {
    response
        .json()
        .await
        .map_err(|_| "Invalid OAuth polling response".into())
}

async fn pause(
    interaction: &Interaction<'_>,
    interval: u64,
    deadline: Instant,
) -> Result<(), String> {
    interaction.cancelled()?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("Device authorization expired; sign in again".into());
    }
    let wait = Duration::from_secs(interval.max(1)).min(remaining);
    tokio::select! {
        result = interaction.pause(wait.as_secs().max(1)) => result?,
        _ = tokio::time::sleep(remaining) => return Err("Device authorization expired; sign in again".into()),
    }
    interaction.cancelled()
}

fn poll_error(raw: &Value, interval: &mut u64) -> Result<(), String> {
    match raw.get("error").and_then(Value::as_str) {
        Some("authorization_pending") => Ok(()),
        Some("slow_down") => {
            *interval = seconds(raw.get("interval"))
                .unwrap_or(interval.saturating_add(5))
                .max(interval.saturating_add(5));
            Ok(())
        }
        Some("access_denied" | "authorization_declined") => {
            Err("Device authorization was denied".into())
        }
        Some("expired_token" | "expired_device_code") => {
            Err("Device authorization expired; sign in again".into())
        }
        _ => Err("Device authorization failed".into()),
    }
}

async fn copilot_login(interaction: &Interaction<'_>) -> Result<Token, String> {
    let input = interaction.input("GitHub Enterprise URL/domain (blank for github.com)")?;
    interaction.cancelled()?;
    let mut domain = if input.trim().is_empty() {
        "github.com".to_owned()
    } else {
        let input = input.trim();
        let url = Url::parse(&if input.contains("://") {
            input.to_owned()
        } else {
            format!("https://{input}")
        })
        .map_err(|_| "Invalid GitHub Enterprise domain".to_owned())?;
        if !url.username().is_empty() || url.password().is_some() || url.scheme() != "https" {
            return Err("Invalid GitHub Enterprise domain".into());
        }
        url.host_str()
            .filter(|host| !host.is_empty())
            .ok_or("Invalid GitHub Enterprise domain")?
            .to_owned()
    };
    if matches!(domain.as_str(), "api.github.com" | "www.github.com") {
        domain = "github.com".into();
    }
    let client = if domain == "github.com" {
        "Ov23li8tweQw6odWQebz"
    } else {
        "Ov23ctDVkRmgkPke0Mmm"
    };
    let http = http()?;
    let device = request_json(
        http.post(format!("https://{domain}/login/device/code"))
            .header("Accept", "application/json")
            .header("User-Agent", "copilot-developer-action/0.0.1")
            .form(&[("client_id", client), ("scope", "read:user")]),
        Some(interaction),
    )
    .await?;
    let code = field(&device, "device_code")?;
    let user_code = field(&device, "user_code")?;
    let url = field(&device, "verification_uri")?;
    let expires = seconds(device.get("expires_in"))
        .filter(|v| *v > 0)
        .ok_or("Device authorization has no expiry")?;
    let mut interval = seconds(device.get("interval")).unwrap_or(5).max(1);
    let deadline = Instant::now() + Duration::from_secs(expires);
    interaction.output(&format!("Enter code: {user_code}"));
    interaction.browser(url);
    loop {
        // GitHub requests a small margin above its advertised minimum interval.
        pause(
            interaction,
            interval.saturating_mul(12).div_ceil(10),
            deadline,
        )
        .await?;
        let response = send(
            http.post(format!("https://{domain}/login/oauth/access_token"))
                .header("Accept", "application/json")
                .header("User-Agent", "copilot-developer-action/0.0.1")
                .form(&[
                    ("client_id", client),
                    ("device_code", code),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ]),
            Some(interaction),
        )
        .await?;
        let status = response.status();
        let raw = poll_json(response).await?;
        if status.is_success() {
            if let Some(token) = raw
                .get("access_token")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                let extra = if domain == "github.com" {
                    json!({})
                } else {
                    json!({"enterpriseUrl": domain})
                };
                return copilot_exchange(token, extra, Some(interaction)).await;
            }
        } else if status.as_u16() != 400 {
            return Err(format!(
                "GitHub device authorization failed (HTTP {status})"
            ));
        }
        poll_error(&raw, &mut interval)?;
    }
}

async fn copilot_exchange(
    github_token: &str,
    mut extra: Value,
    interaction: Option<&Interaction<'_>>,
) -> Result<Token, String> {
    let enterprise = extra
        .get("enterpriseUrl")
        .and_then(Value::as_str)
        .unwrap_or("");
    let github_api = if enterprise.is_empty() {
        "https://api.github.com".to_owned()
    } else {
        format!("https://api.{enterprise}")
    };
    let raw = request_json(
        http()?
            .get(format!("{github_api}/copilot_internal/v2/token"))
            .header("Authorization", format!("token {github_token}"))
            .header("Accept", "application/json")
            .header("User-Agent", "GithubCopilot/1.155.0")
            .header("Editor-Version", "vscode/1.96.2")
            .header("Editor-Plugin-Version", "copilot/1.155.0"),
        interaction,
    )
    .await?;
    let access = field(&raw, "token")?.to_owned();
    let expires = seconds(raw.get("expires_at"))
        .and_then(|v| i64::try_from(v).ok())
        .filter(|v| *v > 0)
        .ok_or("Copilot response is missing token expiry")?;
    let fallback = if enterprise.is_empty() {
        "https://api.githubcopilot.com".to_owned()
    } else if enterprise.starts_with("copilot-api.") {
        format!("https://{enterprise}")
    } else {
        format!("https://copilot-api.{enterprise}")
    };
    let endpoint = raw
        .pointer("/endpoints/api")
        .and_then(Value::as_str)
        .and_then(https_endpoint)
        .or_else(|| {
            access.split(';').find_map(|part| {
                let host = part.strip_prefix("proxy-ep=")?;
                let api_host = host.strip_prefix("proxy.")?;
                https_endpoint(&format!("https://api.{api_host}"))
            })
        })
        .or_else(|| {
            extra
                .get("apiEndpoint")
                .and_then(Value::as_str)
                .and_then(https_endpoint)
        })
        .unwrap_or(fallback);
    if !extra.is_object() {
        extra = json!({});
    }
    extra["apiEndpoint"] = endpoint.into();
    Ok(Token {
        access,
        refresh: github_token.to_owned(),
        expires,
        extra,
    })
}

fn https_endpoint(input: &str) -> Option<String> {
    let url = Url::parse(input).ok()?;
    (url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none())
    .then(|| input.trim_end_matches('/').to_owned())
}

async fn codex_login(interaction: &Interaction<'_>) -> Result<Token, String> {
    let http = http()?;
    let device = request_json(
        http.post("https://auth.openai.com/api/accounts/deviceauth/usercode")
            .json(&json!({"client_id": CODEX_CLIENT})),
        Some(interaction),
    )
    .await?;
    let device_id = field(&device, "device_auth_id")?;
    let user_code = field(&device, "user_code")?;
    let interval = seconds(device.get("interval"))
        .unwrap_or(5)
        .max(1)
        .saturating_add(3);
    let deadline = Instant::now() + Duration::from_secs(30 * 60);
    interaction.output(&format!("Enter code: {user_code}"));
    interaction.browser("https://auth.openai.com/codex/device");
    for attempt in 0..120 {
        pause(
            interaction,
            if attempt == 0 {
                interval.min(5)
            } else {
                interval
            },
            deadline,
        )
        .await?;
        let response = send(
            http.post("https://auth.openai.com/api/accounts/deviceauth/token")
                .json(&json!({"device_auth_id": device_id, "user_code": user_code})),
            Some(interaction),
        )
        .await?;
        if matches!(response.status().as_u16(), 403 | 404) {
            continue;
        }
        let raw = response_json(response).await?;
        let code = field(&raw, "authorization_code")?;
        let verifier = field(&raw, "code_verifier")?;
        let raw = request_json(
            http.post(CODEX_TOKEN).form(&[
                ("grant_type", "authorization_code"),
                ("client_id", CODEX_CLIENT),
                ("code", code),
                ("code_verifier", verifier),
                (
                    "redirect_uri",
                    "https://auth.openai.com/deviceauth/callback",
                ),
            ]),
            Some(interaction),
        )
        .await?;
        return codex_token(raw, None);
    }
    Err("Device authorization timed out; sign in again".into())
}

fn claims(access: &str) -> Value {
    access
        .split('.')
        .nth(1)
        .and_then(|s| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(s.trim_end_matches('='))
                .ok()
        })
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

fn codex_token(raw: Value, previous: Option<&Token>) -> Result<Token, String> {
    let id_claims = raw
        .get("id_token")
        .and_then(Value::as_str)
        .map(claims)
        .unwrap_or(Value::Null);
    if previous.is_none() {
        field(&raw, "refresh_token")?;
        seconds(raw.get("expires_in"))
            .filter(|v| *v > 0)
            .ok_or("OAuth response is missing token lifetime")?;
    }
    let mut token = token_response(raw, previous)?;
    token.extra = previous
        .map(|t| t.extra.clone())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let access_claims = claims(&token.access);
    let account = access_claims
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .or_else(|| id_claims.pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let email = access_claims
        .pointer("/https:~1~1api.openai.com~1profile/email")
        .or_else(|| id_claims.pointer("/https:~1~1api.openai.com~1profile/email"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());
    if previous.is_none() && account.is_none() && email.is_none() {
        return Err("Codex token has no account identity".into());
    }
    if let Some(account) = account {
        token.extra["accountId"] = account.into();
        if previous.is_none() {
            token.extra["orgId"] = account.into();
        }
    }
    if let Some(email) = email {
        token.extra["email"] = email.trim().to_lowercase().into();
    }
    if previous.is_none() {
        if let Some(plan) = access_claims
            .pointer("/https:~1~1api.openai.com~1auth/chatgpt_plan_type")
            .or_else(|| id_claims.pointer("/https:~1~1api.openai.com~1auth/chatgpt_plan_type"))
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        {
            token.extra["orgName"] = plan.trim().to_lowercase().into();
        }
    }
    Ok(token)
}

fn kimi_base() -> String {
    ["KIMI_CODE_OAUTH_HOST", "KIMI_OAUTH_HOST"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| "https://auth.kimi.com".into())
        .trim_end_matches('/')
        .to_owned()
}

fn new_device_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn header_text(value: &str) -> String {
    let clean: String = value
        .chars()
        .filter(|c| c.is_ascii() && !c.is_ascii_control())
        .collect();
    let clean = clean.trim();
    if clean.is_empty() {
        "unknown".into()
    } else {
        clean.to_owned()
    }
}

fn kimi_request(request: RequestBuilder, device: &str) -> RequestBuilder {
    let version = env!("CARGO_PKG_VERSION");
    let hostname = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .unwrap_or_default();
    let os_version = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_else(|_| std::env::consts::OS.into());
    request
        .header("Accept", "application/json")
        .header("User-Agent", format!("KimiCLI/{version}"))
        .header("X-Msh-Platform", "kimi_cli")
        .header("X-Msh-Version", version)
        .header("X-Msh-Device-Name", header_text(&hostname))
        .header(
            "X-Msh-Device-Model",
            header_text(&format!(
                "{} {} {}",
                std::env::consts::OS,
                os_version.trim(),
                std::env::consts::ARCH
            )),
        )
        .header("X-Msh-Os-Version", header_text(&os_version))
        .header("X-Msh-Device-Id", header_text(device))
}

async fn kimi_login(interaction: &Interaction<'_>) -> Result<Token, String> {
    let base = kimi_base();
    let device_id = new_device_id();
    let http = http()?;
    let device = request_json(
        kimi_request(
            http.post(format!("{base}/api/oauth/device_authorization")),
            &device_id,
        )
        .form(&[("client_id", KIMI_CLIENT)]),
        Some(interaction),
    )
    .await?;
    let code = field(&device, "device_code")?;
    let user_code = field(&device, "user_code")?;
    let verification = field(&device, "verification_uri")?;
    let url = device
        .get("verification_uri_complete")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(verification);
    let expires = seconds(device.get("expires_in")).unwrap_or(900).max(1);
    let deadline = Instant::now() + Duration::from_secs(expires);
    let mut interval = seconds(device.get("interval")).unwrap_or(5).max(1);
    interaction.output(&format!("Enter code: {user_code}"));
    interaction.browser(url);
    loop {
        pause(interaction, interval, deadline).await?;
        let response = send(
            kimi_request(http.post(format!("{base}/api/oauth/token")), &device_id).form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", KIMI_CLIENT),
                ("device_code", code),
            ]),
            Some(interaction),
        )
        .await?;
        let status = response.status();
        let raw = poll_json(response).await?;
        if status.is_success() && raw.get("access_token").and_then(Value::as_str).is_some() {
            return kimi_token(raw, None, &base, &device_id);
        }
        if !status.is_success() && status.as_u16() != 400 {
            return Err(format!("Kimi device authorization failed (HTTP {status})"));
        }
        poll_error(&raw, &mut interval)?;
    }
}

fn kimi_token(
    raw: Value,
    previous: Option<&Token>,
    base: &str,
    device: &str,
) -> Result<Token, String> {
    let mut token = token_response(raw, previous)?;
    token.extra = previous
        .map(|t| t.extra.clone())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let payload = claims(&token.access);
    if let Some(account) = payload
        .get("user_id")
        .or_else(|| payload.get("sub"))
        .and_then(Value::as_str)
    {
        token.extra["accountId"] = account.into();
    }
    token.extra["oauthHost"] = base.into();
    token.extra["deviceId"] = device.into();
    Ok(token)
}

fn gitlab_settings(provider: &str) -> (String, String) {
    if provider == "gitlab-duo-agent" {
        (
            GITLAB_AGENT_CLIENT.into(),
            "vscode://gitlab.gitlab-workflow/authentication".into(),
        )
    } else {
        (
            std::env::var("GITLAB_CLIENT_ID")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| GITLAB_CLIENT.into()),
            std::env::var("GITLAB_REDIRECT_URI")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "http://localhost:8080/callback".into()),
        )
    }
}

async fn gitlab_login(provider: &str, interaction: &Interaction<'_>) -> Result<Token, String> {
    let (client, redirect) = gitlab_settings(provider);
    let mut authorize =
        Url::parse("https://gitlab.com/oauth/authorize").map_err(|_| "Invalid GitLab OAuth URL")?;
    authorize
        .query_pairs_mut()
        .append_pair("client_id", &client)
        .append_pair("scope", "api");
    if provider == "gitlab-duo-agent" {
        interaction.output("Complete GitLab login in your browser. If VS Code opens, copy the full vscode://gitlab.gitlab-workflow/authentication callback URL and paste it here.");
    } else {
        interaction.output("Complete GitLab login in your browser. If GitLab rejects the redirect URI, register an OAuth application and set GITLAB_CLIENT_ID and GITLAB_REDIRECT_URI, or use a Personal Access Token via GITLAB_TOKEN.");
    }
    let (code, verifier, _) = browser_code(authorize, &redirect, interaction).await?;
    let raw = request_json(
        http()?.post("https://gitlab.com/oauth/token").form(&[
            ("grant_type", "authorization_code"),
            ("client_id", client.as_str()),
            ("code", code.as_str()),
            ("code_verifier", verifier.as_str()),
            ("redirect_uri", redirect.as_str()),
        ]),
        Some(interaction),
    )
    .await?;
    gitlab_token(raw, None, &client, &redirect)
}

fn gitlab_token(
    raw: Value,
    previous: Option<&Token>,
    client: &str,
    redirect: &str,
) -> Result<Token, String> {
    let lifetime = seconds(raw.get("expires_in")).and_then(|v| i64::try_from(v).ok());
    let created = seconds(raw.get("created_at"))
        .and_then(|v| i64::try_from(v).ok())
        .unwrap_or_else(now);
    let mut token = token_response(raw, previous)?;
    if let Some(lifetime) = lifetime {
        token.expires = created.saturating_add(lifetime).saturating_sub(300);
    }
    token.extra = previous
        .map(|t| t.extra.clone())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    token.extra["clientId"] = client.into();
    token.extra["redirectUri"] = redirect.into();
    Ok(token)
}
