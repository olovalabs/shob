use super::{browser_code, http, now, pkce, response_json, token_response, Interaction, Token};
use base64::Engine;
use reqwest::{Client, RequestBuilder, Url};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const CURSOR_CLIENT: &str = "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB";
const FACTORY_CLIENT: &str = "client_01HNM792M5G5G1A2THWPXKFMXB";
const FACTORY_TOKEN: &str = "https://api.workos.com/user_management/authenticate";
const XAI_CLIENT: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const META_CLIENT: &str = "1031625952748946";

// API-key setup (Alibaba/MiniMax) and imported sessions (devin-cli/Perplexity)
// deliberately do not advertise OAuth support. OpenRouter is declarative.
pub(super) fn supports(provider: &str) -> bool {
    matches!(
        provider,
        "cursor"
            | "devin"
            | "devin-windsurf"
            | "xai-oauth"
            | "factory-droid"
            | "muse-code"
            | "kilo"
            | "zai-coding-plan"
            | "mistral"
            | "snowflake"
    )
}

pub(super) async fn login(provider: &str, interaction: &Interaction<'_>) -> Result<Token, String> {
    interaction.cancelled()?;
    match provider {
        "cursor" => cursor_login(interaction).await,
        "devin" | "devin-windsurf" => devin_login(provider, interaction).await,
        "xai-oauth" | "factory-droid" | "muse-code" => device_login(provider, interaction).await,
        "kilo" => kilo_login(interaction).await,
        "zai-coding-plan" => zai_login(interaction).await,
        "mistral" => mistral_login(interaction).await,
        "snowflake" => snowflake_login(interaction).await,
        "devin-cli" => Err("Devin CLI imports an existing seat API key; it is not an OAuth sign-in. Use devin or devin-windsurf.".into()),
        "perplexity" => Err("Perplexity's upstream hook uses browser sessions or email OTP, not OAuth.".into()),
        "alibaba-coding-plan" | "alibaba-token-plan" | "minimax-code" | "minimax-code-cn" =>
            Err("This provider requires API-key setup, not OAuth sign-in.".into()),
        _ => Err("Unsupported custom OAuth provider".into()),
    }
}

pub(super) async fn refresh(provider: &str, previous: &Token) -> Result<Token, String> {
    match provider {
        "cursor" => {
            require_refresh(previous)?;
            let raw = send_json(
                http()?
                    .post("https://api2.cursor.sh/oauth/token")
                    .json(&json!({
                        "grant_type": "refresh_token", "client_id": CURSOR_CLIENT,
                        "refresh_token": previous.refresh,
                    })),
            )
            .await?;
            if raw["shouldLogout"] == true {
                return Err("Cursor ended this session; sign in again".into());
            }
            let mut token = token_response(raw, Some(previous))?;
            token.expires = jwt_expiry(&token.access).unwrap_or_else(|| now() + 3600);
            cursor_identity(&mut token).await;
            Ok(token)
        }
        "xai-oauth" => {
            require_refresh(previous)?;
            let endpoint = xai_endpoint(&http()?).await?;
            let raw = send_json(
                http()?
                    .post(endpoint)
                    .header("Accept", "application/json")
                    .form(&[
                        ("grant_type", "refresh_token"),
                        ("client_id", XAI_CLIENT),
                        ("refresh_token", previous.refresh.as_str()),
                    ]),
            )
            .await?;
            let mut token = token_response(raw, Some(previous))?;
            xai_identity(&mut token).await;
            Ok(token)
        }
        "factory-droid" => {
            let raw =
                factory_refresh_raw(previous, string(&previous.extra, "activeOrganizationId"))
                    .await?;
            factory_identity(raw, Some(previous), false).await
        }
        "snowflake" => snowflake_refresh(previous).await,
        // These flows mint non-renewable keys or grants. A refresh must not
        // pretend to renew them, even when the stored credential is still valid.
        "devin" | "devin-windsurf" | "muse-code" | "kilo" | "zai-coding-plan" | "mistral" => {
            Err("This sign-in does not support token renewal; sign in again when it expires".into())
        }
        _ => Err("Unsupported custom OAuth refresh provider".into()),
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn required(value: &Value, key: &str) -> Result<String, String> {
    string(value, key)
        .map(str::to_owned)
        .ok_or_else(|| format!("Authentication response is missing {key}"))
}

fn require_refresh(token: &Token) -> Result<(), String> {
    if token.refresh.trim().is_empty() {
        Err("No refresh token was issued; sign in again".into())
    } else {
        Ok(())
    }
}

async fn send(request: RequestBuilder) -> Result<reqwest::Response, String> {
    request
        .send()
        .await
        .map_err(|_| "Authentication network request failed".into())
}

async fn send_json(request: RequestBuilder) -> Result<Value, String> {
    response_json(send(request).await?).await
}

fn url(value: &str) -> Result<Url, String> {
    Url::parse(value).map_err(|_| "Invalid authentication endpoint".into())
}

fn jwt_claims(access: &str) -> Value {
    access
        .split('.')
        .nth(1)
        .and_then(|part| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(part.trim_end_matches('='))
                .ok()
        })
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

fn jwt_expiry(access: &str) -> Option<i64> {
    jwt_claims(access).get("exp")?.as_i64().filter(|v| *v > 0)
}

fn extra(token: &mut Token, key: &str, value: &str) {
    if !token.extra.is_object() {
        token.extra = json!({});
    }
    token.extra[key] = json!(value);
}

fn identity_claims(token: &mut Token) {
    let claims = jwt_claims(&token.access);
    for (claim, field) in [("sub", "accountId"), ("email", "email")] {
        if let Some(value) = string(&claims, claim) {
            extra(token, field, value);
        }
    }
}

async fn cursor_login(interaction: &Interaction<'_>) -> Result<Token, String> {
    let (verifier, challenge) = pkce();
    let id = uuid::Uuid::new_v4().to_string();
    let mut authorize = url("https://cursor.com/loginDeepControl")?;
    authorize.query_pairs_mut().extend_pairs([
        ("challenge", challenge.as_str()),
        ("uuid", id.as_str()),
        ("mode", "login"),
        ("redirectTarget", "cli"),
    ]);
    interaction.browser(authorize.as_str());
    interaction.output("Waiting for Cursor browser authentication...");
    let client = http()?;
    let mut poll = url("https://api2.cursor.sh/auth/poll")?;
    poll.query_pairs_mut()
        .extend_pairs([("uuid", id.as_str()), ("verifier", verifier.as_str())]);
    let mut delay = 1_u64;
    let mut failures = 0;
    for _ in 0..150 {
        interaction.pause(delay).await?;
        let response = send(client.get(poll.clone())).await;
        match response {
            Ok(response) if response.status() == 404 => {
                failures = 0;
                delay = (delay + 1).min(10);
            }
            Ok(response) if response.status().is_success() => {
                let raw = response_json(response).await?;
                let access = required(&raw, "accessToken")?;
                let refresh = required(&raw, "refreshToken")?;
                let expires = jwt_expiry(&access).unwrap_or_else(|| now() + 3600);
                let mut token = Token {
                    access,
                    refresh,
                    expires,
                    extra: json!({}),
                };
                cursor_identity(&mut token).await;
                interaction.cancelled()?;
                return Ok(token);
            }
            _ => {
                failures += 1;
                if failures >= 3 {
                    return Err("Cursor authentication polling failed repeatedly".into());
                }
            }
        }
    }
    Err("Cursor authentication timed out".into())
}

async fn cursor_identity(token: &mut Token) {
    let claims = jwt_claims(&token.access);
    let Some(subject) = string(&claims, "sub") else {
        return;
    };
    extra(
        token,
        "accountId",
        subject.split_once('|').map_or(subject, |(_, id)| id),
    );
    let Ok(client) = http() else { return };
    let result = send_json(
        client
            .post("https://api2.cursor.sh/aiserver.v1.DashboardService/GetMe")
            .timeout(Duration::from_secs(3))
            .bearer_auth(&token.access)
            .header("Connect-Protocol-Version", "1")
            .header("Accept", "application/json")
            .json(&json!({})),
    )
    .await;
    if let Ok(profile) = result {
        if string(&profile, "authId") == Some(subject) {
            if let Some(email) = string(&profile, "email") {
                extra(token, "email", email);
            }
        }
    }
}

async fn devin_login(provider: &str, interaction: &Interaction<'_>) -> Result<Token, String> {
    let windsurf = provider == "devin-windsurf";
    let redirect = if windsurf {
        "http://127.0.0.1:59654/callback"
    } else {
        "http://127.0.0.1:59653/callback"
    };
    let mut authorize = url(if windsurf {
        "https://windsurf.com/editor/signin"
    } else {
        "https://app.devin.ai/auth/cli/continue"
    })?;
    authorize
        .query_pairs_mut()
        .append_pair("prompt", if windsurf { "login" } else { "select_account" });
    if windsurf {
        authorize
            .query_pairs_mut()
            .append_pair("redirect_parameters_type", "query");
    }
    let (code, verifier, _) = browser_code(authorize, redirect, interaction).await?;
    interaction.cancelled()?;
    if !windsurf {
        let raw = send_json(
            http()?
                .post("https://api.devin.ai/auth/cli/token")
                .header("Accept", "application/json")
                .json(&json!({ "code": code, "code_verifier": verifier })),
        )
        .await?;
        let access = required(&raw, "token")?;
        let expires = jwt_expiry(&access).unwrap_or_else(|| now() + 365 * 86400);
        let mut token = Token {
            access,
            refresh: String::new(),
            expires,
            extra: json!({"apiEndpoint": "https://api.devin.ai", "enterpriseUrl": "https://app.devin.ai"}),
        };
        identity_claims(&mut token);
        return Ok(token);
    }
    // The Windsurf exchange is a unary Connect RPC with bare protobuf, not JSON.
    let mut body = Vec::with_capacity(code.len() + verifier.len() + 16);
    proto_string(&mut body, 1, &code);
    proto_string(&mut body, 2, &verifier);
    let response = send(http()?.post("https://server.codeium.com/exa.seat_management_pb.SeatManagementService/ExchangePKCEAuthorizationCode")
        .header("content-type", "application/proto").header("connect-protocol-version", "1")
        .header("accept", "*/*").body(body)).await?;
    if !response.status().is_success() {
        return Err(format!(
            "Windsurf code exchange failed (HTTP {})",
            response.status().as_u16()
        ));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|_| "Could not read Windsurf code exchange".to_owned())?;
    let decoded;
    let payload = if bytes.starts_with(&[0x1f, 0x8b]) {
        use std::io::Read;
        let mut decoder = flate2::read::GzDecoder::new(bytes.as_ref());
        let mut output = Vec::new();
        decoder
            .read_to_end(&mut output)
            .map_err(|_| "Invalid Windsurf compressed response".to_owned())?;
        decoded = output;
        decoded.as_slice()
    } else {
        bytes.as_ref()
    };
    let fields = proto_strings(payload)?;
    let access = fields
        .iter()
        .find(|(number, _)| *number == 1)
        .map(|(_, value)| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .ok_or("Windsurf code exchange returned no API key")?;
    let access = if access.starts_with("gsk-") {
        &access[1..]
    } else {
        access
    };
    let mut metadata = json!({ "apiEndpoint": "https://server.codeium.com", "storeAs": "devin" });
    for (number, value) in &fields {
        let key = match number {
            2 => "name",
            3 => "apiEndpoint",
            4 => "enterpriseUrl",
            5 => "devinApiUrl",
            _ => continue,
        };
        if !value.is_empty() {
            metadata[key] = json!(value);
        }
    }
    interaction.cancelled()?;
    Ok(Token {
        access: access.to_owned(),
        refresh: String::new(),
        expires: 0,
        extra: metadata,
    })
}

fn proto_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 128 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn proto_string(out: &mut Vec<u8>, field: u64, value: &str) {
    proto_varint(out, (field << 3) | 2);
    proto_varint(out, value.len() as u64);
    out.extend_from_slice(value.as_bytes());
}

fn read_varint(bytes: &[u8], position: &mut usize) -> Result<u64, String> {
    let mut value = 0_u64;
    for shift in (0..70).step_by(7) {
        let byte = *bytes
            .get(*position)
            .ok_or("Truncated Windsurf protobuf response")?;
        *position += 1;
        if shift == 63 && byte > 1 {
            return Err("Invalid Windsurf protobuf integer".into());
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err("Invalid Windsurf protobuf integer".into())
}

fn proto_strings(bytes: &[u8]) -> Result<Vec<(u64, String)>, String> {
    let mut position = 0;
    let mut result = Vec::new();
    while position < bytes.len() {
        let tag = read_varint(bytes, &mut position)?;
        if tag >> 3 == 0 {
            return Err("Invalid Windsurf protobuf field".into());
        }
        let count = match tag & 7 {
            0 => {
                read_varint(bytes, &mut position)?;
                continue;
            }
            1 => 8,
            2 => usize::try_from(read_varint(bytes, &mut position)?)
                .map_err(|_| "Invalid Windsurf protobuf length")?,
            5 => 4,
            _ => return Err("Unsupported Windsurf protobuf field".into()),
        };
        let end = position
            .checked_add(count)
            .filter(|end| *end <= bytes.len())
            .ok_or("Truncated Windsurf protobuf response")?;
        if tag & 7 == 2 && (1..=5).contains(&(tag >> 3)) {
            let text = std::str::from_utf8(&bytes[position..end])
                .map_err(|_| "Invalid Windsurf response text")?;
            result.push((tag >> 3, text.to_owned()));
        }
        position = end;
    }
    Ok(result)
}

async fn xai_endpoint(client: &Client) -> Result<Url, String> {
    let document = send_json(
        client
            .get("https://auth.x.ai/.well-known/openid-configuration")
            .timeout(Duration::from_secs(15))
            .header("Accept", "application/json"),
    )
    .await?;
    let endpoint = url(&required(&document, "token_endpoint")?)?;
    let host = endpoint.host_str().ok_or("Invalid xAI token endpoint")?;
    if endpoint.scheme() != "https"
        || !(host == "x.ai" || host.ends_with(".x.ai"))
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
    {
        return Err("xAI discovery returned an untrusted token endpoint".into());
    }
    Ok(endpoint)
}

async fn xai_identity(token: &mut Token) {
    identity_claims(token);
    let Ok(client) = http() else { return };
    if let Ok(profile) = send_json(
        client
            .get("https://auth.x.ai/oauth2/userinfo")
            .header("Accept", "application/json")
            .bearer_auth(&token.access),
    )
    .await
    {
        for (key, field) in [("email", "email"), ("sub", "accountId")] {
            if let Some(value) = string(&profile, key) {
                extra(token, field, value);
            }
        }
    }
}

async fn device_login(provider: &str, interaction: &Interaction<'_>) -> Result<Token, String> {
    let client = http()?;
    let (client_id, device_endpoint, token_endpoint) = match provider {
        "factory-droid" => (
            FACTORY_CLIENT,
            "https://api.workos.com/user_management/authorize/device",
            url(FACTORY_TOKEN)?,
        ),
        "muse-code" => (
            META_CLIENT,
            "https://auth.meta.com/oidc/device/authorization/",
            url("https://auth.meta.com/oidc/device/token/")?,
        ),
        "xai-oauth" => (
            XAI_CLIENT,
            "https://auth.x.ai/oauth2/device/code",
            xai_endpoint(&client).await?,
        ),
        _ => return Err("Unsupported device authorization provider".into()),
    };
    let mut params = vec![("client_id", client_id)];
    if provider == "xai-oauth" {
        params.push((
            "scope",
            "openid profile email offline_access grok-cli:access api:access",
        ));
    }
    let mut request = client
        .post(device_endpoint)
        .header("Accept", "application/json")
        .form(&params);
    if provider == "muse-code" {
        request = request.header("x-api-version", "1.0.0");
    }
    let device = send_json(request).await?;
    let device_code = required(&device, "device_code")?;
    let user_code = required(&device, "user_code")?;
    let verify = string(&device, "verification_uri_complete")
        .or_else(|| string(&device, "verification_uri"))
        .ok_or("Missing device verification URL")?;
    interaction.browser(verify);
    interaction.output(&format!("Enter code: {user_code}"));
    let duration = device["expires_in"]
        .as_u64()
        .filter(|n| *n > 0)
        .unwrap_or(900);
    let start = Instant::now();
    let mut interval = device["interval"].as_u64().unwrap_or(5).max(1);
    while start.elapsed().as_secs() < duration {
        interaction
            .pause(interval.min(duration.saturating_sub(start.elapsed().as_secs())))
            .await?;
        if start.elapsed().as_secs() >= duration {
            break;
        }
        let mut poll = client
            .post(token_endpoint.clone())
            .header("Accept", "application/json")
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", client_id),
                ("device_code", device_code.as_str()),
            ]);
        if provider == "muse-code" {
            poll = poll.header("x-api-version", "1.0.0");
        }
        let response = send(poll).await?;
        let status = response.status();
        let raw: Value = response
            .json()
            .await
            .map_err(|_| "Invalid device token response".to_owned())?;
        match string(&raw, "error") {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval = interval.saturating_add(5);
                continue;
            }
            Some("expired_token") => {
                return Err("Device authorization expired; sign in again".into())
            }
            Some("access_denied") => return Err("Device authorization was denied".into()),
            Some(_) => {
                return Err(format!(
                    "Device token exchange failed (HTTP {})",
                    status.as_u16()
                ))
            }
            None if !status.is_success() => {
                return Err(format!(
                    "Device token exchange failed (HTTP {})",
                    status.as_u16()
                ))
            }
            None => {}
        }
        interaction.cancelled()?;
        return match provider {
            "factory-droid" => factory_identity(raw, None, true).await,
            "muse-code" => muse_key(raw).await,
            _ => {
                let mut token = token_response(raw, None)?;
                xai_identity(&mut token).await;
                Ok(token)
            }
        };
    }
    Err("Device authorization timed out; sign in again".into())
}

async fn muse_key(raw: Value) -> Result<Token, String> {
    let oauth = required(&raw, "access_token")?;
    let payload = send_json(
        http()?
            .post("https://api.meta.ai/muse-code/key")
            .header("Accept", "application/json")
            .header("x-api-version", "1.0.0")
            .bearer_auth(&oauth)
            .json(&json!({ "onboard": true })),
    )
    .await?;
    if payload["is_subs_active"] == false {
        return Err("Muse Code subscription is inactive".into());
    }
    let Some(api_key) = string(&payload, "api_key") else {
        return Err(if payload["require_payment"] == true
            || string(&payload, "action_url").is_some()
            || string(&payload, "require_payment_action_url").is_some()
        {
            "Muse Code subscription payment or onboarding is required"
        } else {
            "Muse Code key exchange returned no API key"
        }
        .into());
    };
    let email = string(&payload, "user_email").map(str::to_lowercase);
    let account = string(&payload, "user_id")
        .or(email.as_deref())
        .ok_or("Muse Code key exchange returned no stable account identity")?;
    let mut metadata = json!({"oauthAccessToken": oauth, "accountId": account});
    if let Some(email) = email.as_deref() {
        metadata["email"] = json!(email);
    }
    // Model requests receive the minted key, never the Meta account token.
    Ok(Token {
        access: api_key.to_owned(),
        refresh: String::new(),
        expires: 0,
        extra: metadata,
    })
}

fn factory_host(metadata: &Value) -> &'static str {
    if string(metadata, "region") == Some("eu") {
        "https://api.eu.factory.ai"
    } else {
        "https://api.factory.ai"
    }
}

async fn factory_refresh_raw(
    previous: &Token,
    organization: Option<&str>,
) -> Result<Value, String> {
    require_refresh(previous)?;
    let mut params = vec![
        ("grant_type", "refresh_token"),
        ("client_id", FACTORY_CLIENT),
        ("refresh_token", previous.refresh.as_str()),
    ];
    if let Some(organization) = organization {
        params.push(("organization_id", organization));
    }
    send_json(
        http()?
            .post(FACTORY_TOKEN)
            .timeout(Duration::from_secs(15))
            .header("Accept", "application/json")
            .form(&params),
    )
    .await
}

async fn factory_whoami(token: &mut Token) -> Result<bool, String> {
    let endpoint = format!("{}/api/cli/whoami", factory_host(&token.extra));
    let mut request = http()?
        .get(endpoint)
        .timeout(Duration::from_secs(15))
        .bearer_auth(&token.access)
        .header("Accept", "application/json")
        .header("X-Factory-Whoami-Extended", "true");
    if let Some(org) = string(&token.extra, "orgId") {
        request = request.header("X-Factory-Org-Id", org);
    }
    let body = send_json(request).await?;
    if let Some(org) = string(&body, "orgId") {
        if string(&token.extra, "orgId").is_some_and(|old| old != org) {
            if let Some(object) = token.extra.as_object_mut() {
                object.remove("region");
                object.remove("inferenceRegion");
            }
        }
        extra(token, "orgId", org);
    }
    if let Some(region @ ("global" | "eu")) = string(&body, "region") {
        extra(token, "region", region);
    }
    if let Some(region @ ("global" | "eu" | "us")) = string(&body, "inferenceRegion") {
        extra(token, "inferenceRegion", region);
    } else if string(&token.extra, "inferenceRegion").is_none() {
        let region = if string(&token.extra, "region") == Some("eu") {
            "eu"
        } else {
            "global"
        };
        extra(token, "inferenceRegion", region);
    }
    let endpoint = factory_host(&token.extra);
    extra(token, "apiEndpoint", endpoint);
    Ok(string(&token.extra, "orgId").is_some())
}

fn factory_token(raw: &Value, previous: Option<&Token>) -> Result<Token, String> {
    required(raw, "refresh_token")?;
    let mut token = token_response(raw.clone(), previous)?;
    token.expires = jwt_expiry(&token.access).unwrap_or_else(|| now() + 86400);
    let claims = jwt_claims(&token.access);
    let selected = string(raw, "organization_id")
        .or_else(|| previous.and_then(|p| string(&p.extra, "activeOrganizationId")));
    let claimed_org = string(&claims, "external_org_id");
    let same_org = previous.is_none_or(|p| {
        !claimed_org
            .zip(string(&p.extra, "orgId"))
            .is_some_and(|(a, b)| a != b)
            && !selected
                .zip(string(&p.extra, "activeOrganizationId"))
                .is_some_and(|(a, b)| a != b)
    });
    if !same_org {
        if let Some(object) = token.extra.as_object_mut() {
            for field in ["orgId", "region", "inferenceRegion", "apiEndpoint"] {
                object.remove(field);
            }
        }
    }
    if let Some(org) = selected {
        extra(&mut token, "activeOrganizationId", org);
    }
    if let Some(org) = claimed_org {
        extra(&mut token, "orgId", org);
    }
    identity_claims(&mut token);
    for (source, target) in [("email", "email"), ("id", "accountId")] {
        if let Some(value) = raw.get("user").and_then(|user| string(user, source)) {
            extra(&mut token, target, value);
        }
    }
    Ok(token)
}

async fn factory_identity(
    raw: Value,
    previous: Option<&Token>,
    login: bool,
) -> Result<Token, String> {
    let mut token = factory_token(&raw, previous)?;
    let claim_has_org = string(&jwt_claims(&token.access), "external_org_id").is_some();
    let checked = factory_whoami(&mut token).await;
    let accepted = matches!(&checked, Ok(true));
    if !claim_has_org && !accepted && string(&token.extra, "activeOrganizationId").is_none() {
        let orgs = send_json(
            http()?
                .get(format!("{}/api/cli/org", factory_host(&token.extra)))
                .timeout(Duration::from_secs(15))
                .bearer_auth(&token.access)
                .header("Accept", "application/json"),
        )
        .await;
        if let Ok(orgs) = orgs {
            if let Some(org) = orgs["workosOrgIds"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                // Scope the rotating refresh grant once. Never reuse the old
                // refresh token after a successful scoped exchange.
                let scoped_raw = factory_refresh_raw(&token, Some(org)).await?;
                let mut scoped = factory_token(&scoped_raw, Some(&token))?;
                extra(&mut scoped, "activeOrganizationId", org);
                let confirmed = factory_whoami(&mut scoped).await;
                if login && !matches!(confirmed, Ok(true)) {
                    return Err("Factory login did not resolve an accepted organization".into());
                }
                return Ok(scoped);
            }
        }
    }
    if login {
        checked?;
        if string(&token.extra, "orgId").is_none() {
            return Err("Factory login did not resolve an organization".into());
        }
    }
    Ok(token)
}

async fn kilo_login(interaction: &Interaction<'_>) -> Result<Token, String> {
    let client = http()?;
    let device = send_json(
        client
            .post("https://api.kilo.ai/api/device-auth/codes")
            .header("Content-Type", "application/json"),
    )
    .await?;
    let code = required(&device, "code")?;
    let verify = required(&device, "verificationUrl")?;
    let duration = device["expiresIn"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or("Kilo device response is missing a valid expiry")?;
    interaction.browser(&verify);
    interaction.output(&format!("Enter code: {code}"));
    let mut poll = url("https://api.kilo.ai/api/device-auth/codes/")?;
    poll.path_segments_mut()
        .map_err(|_| "Invalid Kilo endpoint")?
        .pop_if_empty()
        .push(&code);
    let start = Instant::now();
    while start.elapsed().as_secs() < duration {
        interaction.cancelled()?;
        let response = send(client.get(poll.clone())).await?;
        match response.status().as_u16() {
            202 => {}
            403 => return Err("Kilo authorization was denied".into()),
            410 => return Err("Kilo authorization expired".into()),
            _ => {
                let raw = response_json(response).await?;
                match string(&raw, "status") {
                    Some("approved") => {
                        return Ok(Token {
                            access: required(&raw, "token")?,
                            refresh: String::new(),
                            expires: now() + 365 * 86400,
                            extra: json!({}),
                        })
                    }
                    Some("denied") => return Err("Kilo authorization was denied".into()),
                    Some("expired") => return Err("Kilo authorization expired".into()),
                    _ => {}
                }
            }
        }
        interaction
            .pause(5.min(duration.saturating_sub(start.elapsed().as_secs())))
            .await?;
    }
    Err("Kilo authorization timed out".into())
}

fn setting(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.into())
}

fn zai_envelope(value: Value) -> Result<Value, String> {
    if value.get("code").is_some() || value.get("success").is_some() {
        if value["success"] == false
            || value.get("code").is_some_and(|code| {
                !code.is_null() && code != 0 && code != 200 && code != "0" && code != "200"
            })
        {
            return Err("Z.ai business operation was rejected".into());
        }
        if let Some(data) = value.get("data") {
            return Ok(data.clone());
        }
    }
    Ok(value)
}

async fn zai_login(interaction: &Interaction<'_>) -> Result<Token, String> {
    let redirect = setting("ZAI_OAUTH_REDIRECT_URI", "zcode://zai-auth/callback");
    let state = uuid::Uuid::new_v4().to_string();
    let mut authorize = url(&setting(
        "ZAI_OAUTH_AUTHORIZE_URL",
        "https://chat.z.ai/api/oauth/authorize",
    ))?;
    let client_id = setting("ZAI_OAUTH_CLIENT_ID", "client_P8X5CMWmlaRO9gyO-KSqtg");
    authorize.query_pairs_mut().extend_pairs([
        ("client_id", client_id.as_str()),
        ("response_type", "code"),
        ("redirect_uri", redirect.as_str()),
        ("state", state.as_str()),
    ]);
    // Z.ai rejects loopback callbacks and PKCE for its desktop client. Manual
    // pasted redirects retain state validation without replacing OS handlers.
    interaction.browser(authorize.as_str());
    interaction.output(
        "Complete Z.ai login, then paste the final zcode:// redirect URL (or authorization code).",
    );
    let input = interaction.input("Z.ai redirect URL or authorization code")?;
    interaction.cancelled()?;
    let input = input.trim();
    if input.is_empty() {
        return Err("Z.ai authorization code is required".into());
    }
    let code = if input.contains("://") {
        let callback = url(input)?;
        let expected = url(&redirect)?;
        if callback.scheme() != expected.scheme()
            || callback.host_str() != expected.host_str()
            || callback.path() != expected.path()
            || callback.port() != expected.port()
        {
            return Err("Unexpected Z.ai callback URL".into());
        }
        let params: std::collections::HashMap<_, _> = callback.query_pairs().into_owned().collect();
        if params.get("state") != Some(&state) {
            return Err("Z.ai callback state mismatch".into());
        }
        if params.contains_key("error") {
            return Err("Z.ai authorization was denied".into());
        }
        params
            .get("code")
            .filter(|c| !c.is_empty())
            .cloned()
            .ok_or("Z.ai callback is missing its code")?
    } else {
        input.to_owned()
    };
    let client = http()?;
    let raw = send_json(client.post(setting("ZAI_OAUTH_TOKEN_URL", "https://zcode.z.ai/api/v1/oauth/token"))
        .json(&json!({ "provider": "zai", "code": code, "redirect_uri": redirect, "state": state }))).await?;
    let account_oauth = raw
        .pointer("/data/zai/access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("Z.ai exchange returned no OAuth access token")?;
    let business = zai_envelope(
        send_json(
            client
                .post(setting(
                    "ZAI_BUSINESS_LOGIN_URL",
                    "https://api.z.ai/api/auth/z/login",
                ))
                .json(&json!({"token": account_oauth})),
        )
        .await?,
    )?;
    let business_token = string(&business, "access_token")
        .or_else(|| string(&business, "accessToken"))
        .ok_or("Z.ai business login returned no token")?;
    let base = setting("ZAI_BIZ_BASE", "https://api.z.ai");
    let customer = zai_envelope(
        send_json(
            client
                .get(format!(
                    "{}/api/biz/customer/getCustomerInfo",
                    base.trim_end_matches('/')
                ))
                .bearer_auth(business_token),
        )
        .await?,
    )?;
    let organizations = customer["organizations"]
        .as_array()
        .ok_or("Z.ai account has no organization")?;
    let organization = organizations
        .iter()
        .find(|o| o["isDefault"] == true)
        .or_else(|| organizations.first())
        .ok_or("Z.ai account has no organization")?;
    let projects = organization["projects"]
        .as_array()
        .ok_or("Z.ai account has no project")?;
    let project = projects
        .iter()
        .find(|p| p["isDefault"] == true)
        .or_else(|| projects.first())
        .ok_or("Z.ai account has no project")?;
    let organization_id = required(organization, "organizationId")?;
    let project_id = required(project, "projectId")?;
    let mut keys_url = url(&base)?;
    keys_url
        .path_segments_mut()
        .map_err(|_| "Invalid Z.ai business endpoint")?
        .clear()
        .extend([
            "api",
            "biz",
            "v1",
            "organization",
            organization_id.as_str(),
            "projects",
            project_id.as_str(),
            "api_keys",
        ]);
    let list =
        zai_envelope(send_json(client.get(keys_url.clone()).bearer_auth(business_token)).await?)?;
    let keys = list.as_array().or_else(|| {
        ["list", "keys", "apiKeys", "records"]
            .iter()
            .find_map(|field| list.get(field).and_then(Value::as_array))
    });
    let existing = keys.and_then(|keys| {
        keys.iter()
            .find(|key| string(key, "name") == Some("kathamo"))
    });
    let key = if let Some(existing) = existing {
        existing.clone()
    } else {
        zai_envelope(
            send_json(
                client
                    .post(keys_url.clone())
                    .bearer_auth(business_token)
                    .json(&json!({"name": "kathamo"})),
            )
            .await?,
        )?
    };
    let key_id = required(&key, "apiKey")?;
    keys_url
        .path_segments_mut()
        .map_err(|_| "Invalid Z.ai key endpoint")?
        .extend(["copy", key_id.as_str()]);
    let copied = zai_envelope(send_json(client.get(keys_url).bearer_auth(business_token)).await?)?;
    let secret = required(&copied, "secretKey")?;
    let mut metadata =
        json!({"organizationId": organization_id, "projectId": project_id, "storeAs": "zai"});
    for (pointer, field) in [
        ("/data/user/email", "email"),
        ("/data/user/id", "accountId"),
    ] {
        if let Some(value) = raw.pointer(pointer).and_then(Value::as_str) {
            metadata[field] = json!(value);
        }
    }
    interaction.cancelled()?;
    Ok(Token {
        access: format!("{key_id}.{secret}"),
        refresh: String::new(),
        expires: 0,
        extra: metadata,
    })
}

fn mistral_url(value: &str, api: bool) -> Result<Url, String> {
    let endpoint = url(value)?;
    if endpoint.scheme() != "https"
        || endpoint.host_str() != Some("console.mistral.ai")
        || endpoint.port_or_known_default() != Some(443)
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || (api && endpoint.path() != "/api" && !endpoint.path().starts_with("/api/"))
    {
        return Err("Mistral returned an untrusted sign-in endpoint".into());
    }
    Ok(endpoint)
}

// RFC 3339 expires_at from Mistral. Civil-date conversion keeps auth independent
// of optional date/time crates; offsets and fractional seconds are accepted.
fn rfc3339_seconds(value: &str) -> Option<i64> {
    let (date, time) = value.split_once('T').or_else(|| value.split_once('t'))?;
    let mut date = date.split('-');
    let mut year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    if date.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=days).contains(&day) {
        return None;
    }
    let (clock, offset) =
        if let Some(clock) = time.strip_suffix('Z').or_else(|| time.strip_suffix('z')) {
            (clock, 0)
        } else {
            let position = time.find(['+', '-'])?;
            let (clock, zone) = time.split_at(position);
            let (hours, minutes) = zone[1..].split_once(':')?;
            let hours: i64 = hours.parse().ok()?;
            let minutes: i64 = minutes.parse().ok()?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            (
                clock,
                (hours * 3600 + minutes * 60) * if zone.starts_with('-') { -1 } else { 1 },
            )
        };
    let mut clock = clock.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.split('.').next()?.parse().ok()?;
    if clock.next().is_some()
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    year -= i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * adjusted_month + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146097 + doe - 719468) * 86400 + hour * 3600 + minute * 60 + second - offset)
}

async fn mistral_login(interaction: &Interaction<'_>) -> Result<Token, String> {
    let client = http()?;
    let (verifier, challenge) = pkce();
    let started = send_json(
        client
            .post("https://console.mistral.ai/api/vibe/sign-in")
            .json(&json!({"code_challenge": challenge, "code_challenge_method": "S256"})),
    )
    .await?;
    let process = required(&started, "process_id")?;
    let authorize = mistral_url(&required(&started, "sign_in_url")?, false)?;
    let poll = mistral_url(&required(&started, "poll_url")?, true)?;
    let deadline = rfc3339_seconds(&required(&started, "expires_at")?)
        .ok_or("Mistral returned an invalid sign-in expiry")?;
    interaction.browser(authorize.as_str());
    interaction.output("Sign in with your Mistral account and approve Mistral Vibe access.");
    let mut failures = 0;
    let mut delay = 3;
    while now() < deadline {
        interaction
            .pause(delay.min((deadline - now()).max(0) as u64))
            .await?;
        if now() >= deadline {
            break;
        }
        let result = send(client.get(poll.clone())).await;
        let response = match result {
            Ok(response) => response,
            Err(error) => {
                failures += 1;
                if failures >= 3 {
                    return Err(error);
                }
                continue;
            }
        };
        match response.status().as_u16() {
            410 => return Err("Mistral sign-in expired".into()),
            429 => {
                failures = 0;
                delay = response
                    .headers()
                    .get("retry-after")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(8)
                    .clamp(3, 30);
                continue;
            }
            _ => {}
        }
        let status = response.status();
        let payload = response.json::<Value>().await;
        let raw = match payload {
            Ok(raw) if status.is_success() && string(&raw, "status").is_some() => raw,
            _ => {
                failures += 1;
                if failures >= 3 {
                    return Err("Mistral sign-in status unavailable".into());
                }
                continue;
            }
        };
        failures = 0;
        delay = 3;
        match string(&raw, "status") {
            Some("pending") => continue,
            Some("completed") => {
                let exchange = required(&raw, "exchange_token")?;
                let mut endpoint = url("https://console.mistral.ai/api/vibe/sign-in/")?;
                endpoint
                    .path_segments_mut()
                    .map_err(|_| "Invalid Mistral exchange endpoint")?
                    .pop_if_empty()
                    .push(&process)
                    .push("exchange");
                let response = send(
                    client
                        .post(endpoint)
                        .json(&json!({"exchange_token": exchange,
                    "code_verifier": verifier})),
                )
                .await?;
                let status = response.status();
                let key: Value = response
                    .json()
                    .await
                    .map_err(|_| "Invalid Mistral key exchange response")?;
                if status.as_u16() == 403 && key["error_code"] == 9004 {
                    return Err("This Mistral workspace cannot mint Vibe API keys; sign in and choose another workspace".into());
                }
                if !status.is_success() {
                    return Err(format!(
                        "Mistral key exchange failed (HTTP {})",
                        status.as_u16()
                    ));
                }
                interaction.cancelled()?;
                return Ok(Token {
                    access: required(&key, "api_key")?,
                    refresh: String::new(),
                    expires: 0,
                    extra: json!({"authMethod": "vibe-sign-in"}),
                });
            }
            Some("denied") => return Err("Mistral sign-in was denied".into()),
            Some("expired") => return Err("Mistral sign-in expired".into()),
            _ => return Err("Mistral sign-in failed".into()),
        }
    }
    Err("Mistral sign-in expired".into())
}

fn dns_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && label
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && label
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && label
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

fn snowflake_account(value: &str) -> Result<String, String> {
    let value = value.trim().to_ascii_lowercase().replace('_', "-");
    let invalid = "Enter a Snowflake account identifier (orgname-accountname) or HTTPS account URL";
    if value.is_empty() || value.contains('\\') {
        return Err(invalid.into());
    }
    let mut host = if value.contains("://") {
        let parsed = url(&value)?;
        if parsed.scheme() != "https"
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.port_or_known_default() != Some(443)
        {
            return Err(invalid.into());
        }
        if parsed.host_str() == Some("app.snowflake.com") {
            let parts: Vec<_> = parsed
                .path()
                .split('/')
                .filter(|part| !part.is_empty())
                .collect();
            let org = *parts.first().ok_or(invalid)?;
            let account = *parts.get(1).ok_or(invalid)?;
            if !dns_label(org) || !dns_label(account) {
                return Err(invalid.into());
            }
            // Legacy cloud-region/locator links do not identify the host.
            let segments: Vec<_> = org.split('-').collect();
            if segments.len() >= 3
                && segments.first().is_some_and(|s| s.len() == 2)
                && segments
                    .last()
                    .is_some_and(|s| s.bytes().all(|c| c.is_ascii_digit()))
            {
                return Err("Legacy Snowsight links do not identify the account; enter the account host instead".into());
            }
            format!("{org}-{account}.snowflakecomputing.com")
        } else {
            parsed.host_str().ok_or(invalid)?.to_owned()
        }
    } else {
        if value.contains(['/', '?', '#']) {
            return Err(invalid.into());
        }
        value.strip_suffix(":443").unwrap_or(&value).to_owned()
    };
    if host.ends_with(".snowflakecomputing.cn") {
        return Err("Snowflake Cortex is unavailable for China-region accounts".into());
    }
    if !host.ends_with(".snowflakecomputing.com") {
        if value.contains("://") {
            return Err(invalid.into());
        }
        host.push_str(".snowflakecomputing.com");
    }
    if host == "snowflakecomputing.com" || !host.split('.').all(dns_label) {
        return Err(invalid.into());
    }
    Ok(format!("https://{host}"))
}

fn snowflake_token(raw: Value, previous: Option<&Token>, account: &str) -> Result<Token, String> {
    if raw["expires_in"]
        .as_f64()
        .is_none_or(|seconds| !seconds.is_finite() || seconds <= 0.0)
    {
        return Err("Snowflake returned an invalid token lifetime".into());
    }
    if raw.get("refresh_token").is_some() && string(&raw, "refresh_token").is_none() {
        return Err("Snowflake returned an invalid refresh token".into());
    }
    let mut token = token_response(raw, previous)?;
    extra(&mut token, "enterpriseUrl", account);
    extra(&mut token, "apiEndpoint", account);
    extra(&mut token, "account", account);
    Ok(token)
}

async fn snowflake_login(interaction: &Interaction<'_>) -> Result<Token, String> {
    let account = interaction.input("Enter your Snowflake account identifier or URL")?;
    interaction.cancelled()?;
    let account = snowflake_account(&account)?;
    let mut authorize = url(&format!("{account}/oauth/authorize"))?;
    authorize.query_pairs_mut().extend_pairs([
        ("client_id", "LOCAL_APPLICATION"),
        ("scope", "refresh_token"),
    ]);
    let redirect = "http://127.0.0.1:54551/";
    let (code, verifier, _) = browser_code(authorize, redirect, interaction).await?;
    let raw = send_json(
        http()?
            .post(format!("{account}/oauth/token-request"))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code.as_str()),
                ("redirect_uri", redirect),
                ("code_verifier", verifier.as_str()),
                ("client_id", "LOCAL_APPLICATION"),
            ]),
    )
    .await?;
    interaction.cancelled()?;
    snowflake_token(raw, None, &account)
}

async fn snowflake_refresh(previous: &Token) -> Result<Token, String> {
    if previous.refresh.trim().is_empty() {
        // A grant without refresh is still usable until its actual expiry.
        if previous.expires > now() {
            return Ok(Token {
                access: previous.access.clone(),
                refresh: String::new(),
                expires: previous.expires,
                extra: previous.extra.clone(),
            });
        }
        return Err("Snowflake did not issue a refresh token; sign in again".into());
    }
    let account = snowflake_account(
        string(&previous.extra, "enterpriseUrl").ok_or("Missing Snowflake account metadata")?,
    )?;
    let raw = send_json(
        http()?
            .post(format!("{account}/oauth/token-request"))
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", previous.refresh.as_str()),
                ("client_id", "LOCAL_APPLICATION"),
            ]),
    )
    .await?;
    snowflake_token(raw, Some(previous), &account)
}
