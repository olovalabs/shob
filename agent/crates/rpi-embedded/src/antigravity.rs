//! In-process browser OAuth and token renewal for Google Antigravity.
//! Protocol reference: Oh My Pi's Google Antigravity OAuth implementation (MIT).

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rpi_ai::providers::antigravity::{self, Account, AntigravityProvider, ENDPOINT, ID};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{EnvironmentValues, LoginEvent};

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REDIRECT_URI: &str = "http://127.0.0.1:51121/oauth-callback";
const SCOPES: &str = concat!(
    "https://www.googleapis.com/auth/cloud-platform ",
    "https://www.googleapis.com/auth/userinfo.email ",
    "https://www.googleapis.com/auth/userinfo.profile ",
    "https://www.googleapis.com/auth/cclog ",
    "https://www.googleapis.com/auth/experimentsandconfigs ",
    "https://www.googleapis.com/auth/aicode",
);

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Credentials {
    access: String,
    refresh: String,
    expires: i64,
    project_id: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
}

fn auth_path() -> Result<PathBuf, String> {
    Ok(rpi_cli::config::agent_dir()
        .map_err(|error| error.to_string())?
        .join("antigravity-auth.json"))
}

fn read(path: &Path) -> Result<Option<Credentials>, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| "Invalid Antigravity credentials. Sign in again using /auth.".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Could not read Antigravity credentials: {error}")),
    }
}

fn locked<T>(path: &Path, action: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let acquire = || -> std::io::Result<_> {
        fs::create_dir_all(path.parent().unwrap())?;
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path.with_extension("lock"))?;
        lock.lock()?;
        Ok(lock)
    };
    let _lock =
        acquire().map_err(|error| format!("Could not lock Antigravity credentials: {error}"))?;
    action()
}

fn write(path: &Path, credential: &Credentials) -> Result<(), String> {
    let save = || -> Result<(), Box<dyn std::error::Error>> {
        let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        serde_json::to_writer(&mut temp, credential)?;
        temp.persist(path)?;
        Ok(())
    };
    save().map_err(|error| format!("Could not save Antigravity credentials: {error}"))
}

pub(crate) fn ready() -> bool {
    auth_path()
        .ok()
        .and_then(|path| read(&path).ok().flatten())
        .is_some()
}

pub(crate) fn disconnect() -> Result<(), String> {
    let path = auth_path()?;
    locked(&path, || match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not disconnect Antigravity: {error}")),
    })
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_secs() as i64)
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(25))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("Could not create Antigravity HTTP client: {error}"))
}

#[derive(Deserialize)]
struct OAuthClient {
    client_id: String,
    client_secret: String,
}

impl OAuthClient {
    fn load() -> Result<Self, String> {
        let path = auth_path()?.with_file_name("antigravity-oauth-client.json");
        let client: Self = match (
            std::env::var("KATHAMO_ANTIGRAVITY_CLIENT_ID").ok(),
            std::env::var("KATHAMO_ANTIGRAVITY_CLIENT_SECRET").ok(),
        ) {
            (Some(client_id), Some(client_secret)) => Self {
                client_id,
                client_secret,
            },
            (None, None) => {
                let bytes = fs::read(path).map_err(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        "Configure the Antigravity OAuth client in antigravity-oauth-client.json or set KATHAMO_ANTIGRAVITY_CLIENT_ID and KATHAMO_ANTIGRAVITY_CLIENT_SECRET.".into()
                    } else {
                        format!("Could not read the Antigravity OAuth client configuration: {error}")
                    }
                })?;
                serde_json::from_slice(&bytes)
                    .map_err(|_| "Invalid Antigravity OAuth client configuration.")?
            }
            _ => return Err("Set both Antigravity OAuth client environment variables.".into()),
        };
        if client.client_id.trim().is_empty() || client.client_secret.trim().is_empty() {
            return Err("Antigravity OAuth client ID and secret must be nonempty.".into());
        }
        Ok(client)
    }
}

async fn token_exchange(
    http: &reqwest::Client,
    client: &OAuthClient,
    fields: &[(&str, &str)],
) -> Result<TokenResponse, String> {
    let mut form = vec![
        ("client_id", client.client_id.as_str()),
        ("client_secret", client.client_secret.as_str()),
    ];
    form.extend_from_slice(fields);
    let response = http
        .post(TOKEN_URL)
        .form(&form)
        .send()
        .await
        .map_err(|error| format!("Antigravity token exchange failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "Antigravity token exchange returned HTTP {}. Sign in again using /auth.",
            response.status()
        ));
    }
    response
        .json()
        .await
        .map_err(|_| "Google returned an invalid token response.".into())
}

async fn account(
    http: &reqwest::Client,
    path: &Path,
    rejected: Option<String>,
) -> Result<Account, String> {
    static REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let needs_refresh = |credentials: &Credentials| {
        credentials.expires <= now() + 60 || rejected.as_ref() == Some(&credentials.access)
    };
    let load =
        || read(path)?.ok_or_else(|| "Sign in to Google Antigravity using /auth.".to_owned());
    let mut credential = load()?;
    if needs_refresh(&credential) {
        let _refresh = REFRESH.lock().await;
        credential = load()?;
        if needs_refresh(&credential) {
            let tokens = token_exchange(
                http,
                &OAuthClient::load()?,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &credential.refresh),
                ],
            )
            .await?;
            credential = locked(path, || {
                let latest = load()?;
                if latest.access != credential.access || latest.refresh != credential.refresh {
                    return Ok(latest);
                }
                let updated = Credentials {
                    access: tokens.access_token,
                    refresh: tokens.refresh_token.unwrap_or(credential.refresh),
                    expires: now().saturating_add(tokens.expires_in),
                    project_id: credential.project_id,
                };
                write(path, &updated)?;
                Ok(updated)
            })?;
        }
    }
    Ok(Account {
        access_token: credential.access,
        project_id: credential.project_id,
    })
}

pub(crate) fn models() -> Vec<rpi_ai::Model> {
    let mut models = antigravity::models();
    if let Some(provider) = rpi_runtime::catalog::builtin_provider(ID) {
        for model in &provider.models {
            if !models.iter().any(|known| known.id == model.id) {
                let mut model = model.clone();
                model.api = rpi_ai::Api::GoogleGenerativeAi;
                model.base_url = ENDPOINT.into();
                models.push(model);
            }
        }
    }
    models
}

pub(crate) fn resolve(model: Option<&str>) -> Result<rpi_runtime::provider::ResolvedModel, String> {
    let path = auth_path()?;
    if read(&path)?.is_none() {
        return Err("Sign in to Google Antigravity using /auth.".into());
    }
    let http = http_client()?;
    let account_http = http.clone();
    let source = Arc::new(move |rejected| {
        let path = path.clone();
        let http = account_http.clone();
        Box::pin(async move { account(&http, &path, rejected).await }) as _
    });
    let mut factories = rpi_runtime::provider::RuntimeFactories::default();
    let descriptors = models();
    let api = descriptors[0].api.clone();
    let provider = AntigravityProvider::with_models(http, source, descriptors.clone());
    factories.declare(rpi_runtime::ProviderDeclaration {
        id: ID.into(),
        models: descriptors,
        api_key_env: None,
        requires_auth: false,
    });
    factories.register(Arc::new(provider), api);
    rpi_runtime::RuntimeConfig::default()
        .resolve(
            &rpi_runtime::ModelRequest {
                provider: Some(ID),
                model: model.or(Some(antigravity::MODEL)),
                ..Default::default()
            },
            &factories,
            &EnvironmentValues,
        )
        .map_err(|error| error.to_string())
}

fn authorize_url(client_id: &str, state: &str, verifier: &str) -> reqwest::Url {
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier));
    let mut url = reqwest::Url::parse("https://accounts.google.com/o/oauth2/v2/auth").unwrap();
    url.query_pairs_mut().extend_pairs([
        ("client_id", client_id),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPES),
        ("response_type", "code"),
        ("state", state),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("access_type", "offline"),
        ("prompt", "consent"),
    ]);
    url
}

fn callback(stream: &mut TcpStream, expected_state: &str) -> Option<Result<String, String>> {
    stream.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(1)))
        .ok()?;
    let mut line = String::new();
    BufReader::new(&mut *stream)
        .take(8192)
        .read_line(&mut line)
        .ok()?;
    let mut request = line.split_ascii_whitespace();
    if request.next() != Some("GET") {
        return None;
    }
    let target = request.next()?.strip_prefix("/oauth-callback?")?;
    let url = reqwest::Url::parse(&format!("{REDIRECT_URI}?{target}")).ok()?;
    let query = url
        .query_pairs()
        .collect::<std::collections::BTreeMap<_, _>>();
    if query.get("state").map(|value| value.as_ref()) != Some(expected_state) {
        let _ = stream.write_all(
            b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        );
        return None;
    }
    let reply = "You can close this tab and return to Kathamo.";
    let _ = write!(stream, "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{reply}", reply.len());
    Some(
        query
            .get("code")
            .filter(|code| !code.is_empty())
            .map(|value| value.to_string())
            .ok_or_else(|| "Google sign-in was denied or cancelled.".into()),
    )
}

fn check_cancelled(input: &mpsc::Receiver<String>) -> Result<(), String> {
    if matches!(input.try_recv(), Err(mpsc::TryRecvError::Disconnected)) {
        Err("Antigravity sign-in was cancelled.".into())
    } else {
        Ok(())
    }
}

async fn project(http: &reqwest::Client, token: &str) -> Result<String, String> {
    let payload = json!({"metadata": {"ideType": "ANTIGRAVITY"}});
    let request = || {
        http.post(format!("{ENDPOINT}/v1internal:loadCodeAssist"))
            .header("user-agent", antigravity::user_agent())
            .bearer_auth(token)
            .json(&payload)
    };
    let decode = async |request: reqwest::RequestBuilder| -> Result<Value, String> {
        let response = request
            .send()
            .await
            .map_err(|error| format!("Antigravity account lookup failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "Antigravity account lookup returned HTTP {}",
                response.status()
            ));
        }
        response
            .json()
            .await
            .map_err(|_| "Antigravity returned an invalid project response.".into())
    };
    let mut status = decode(request()).await?;
    if let Some(id) = status["cloudaicompanionProject"]
        .as_str()
        .filter(|id| !id.is_empty())
    {
        return Ok(id.to_owned());
    }
    if !status["allowedTiers"]
        .as_array()
        .is_some_and(|tiers| tiers.iter().any(|tier| tier["id"] == "free-tier"))
    {
        return Err("Antigravity did not return a project for this Google account.".into());
    }
    let service = |request: reqwest::RequestBuilder| {
        request
            .header("user-agent", antigravity::user_agent())
            .bearer_auth(token)
    };
    let mut operation = decode(
        service(http.post(format!("{ENDPOINT}/v1internal:onboardUser")))
            .json(&json!({"tierId":"free-tier", "metadata": {"ideType":"ANTIGRAVITY"}})),
    )
    .await?;
    for _ in 0..15 {
        if operation["done"] == true {
            if !operation["error"].is_null() {
                return Err("Antigravity could not provision this account.".into());
            }
            status = decode(request()).await?;
            return status["cloudaicompanionProject"]
                .as_str()
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| "Antigravity did not return a project after provisioning.".into());
        }
        let name = operation["name"]
            .as_str()
            .filter(|name| {
                name.starts_with("operations/")
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"/_-".contains(&byte))
            })
            .ok_or("Antigravity returned an invalid provisioning operation.")?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        operation = decode(service(http.get(format!("{ENDPOINT}/v1internal/{name}")))).await?;
    }
    Err("Antigravity project provisioning timed out.".into())
}

pub(crate) fn login(
    events: &mpsc::Sender<LoginEvent>,
    input: &mpsc::Receiver<String>,
) -> Result<(), String> {
    let client = OAuthClient::load()?;
    let listener = TcpListener::bind(("127.0.0.1", 51121))
        .map_err(|error| format!("Could not start Antigravity's OAuth callback: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let state = Uuid::new_v4().simple().to_string();
    let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let url = authorize_url(&client.client_id, &state, &verifier);
    events
        .send(LoginEvent::BrowserUrl(url.to_string()))
        .map_err(|_| "The sign-in dialog was closed.")?;
    let deadline = Instant::now() + Duration::from_secs(300);
    let code = loop {
        check_cancelled(input)?;
        if Instant::now() >= deadline {
            return Err("Antigravity sign-in timed out.".into());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                if let Some(code) = callback(&mut stream, &state) {
                    break code?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(30))
            }
            Err(error) => return Err(format!("Antigravity callback failed: {error}")),
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let credential = runtime.block_on(async {
        let http = http_client()?;
        let tokens = token_exchange(
            &http,
            &client,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", REDIRECT_URI),
                ("code_verifier", &verifier),
            ],
        )
        .await?;
        let refresh = tokens
            .refresh_token
            .ok_or("Google did not issue a refresh token.")?;
        let project_id = tokio::time::timeout(
            Duration::from_secs(40),
            project(&http, &tokens.access_token),
        )
        .await
        .map_err(|_| "Antigravity project lookup timed out.")??;
        Ok::<_, String>(Credentials {
            access: tokens.access_token,
            refresh,
            project_id,
            expires: now().saturating_add(tokens.expires_in),
        })
    })?;
    check_cancelled(input)?;
    let path = auth_path()?;
    locked(&path, || write(&path, &credential))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_requires_the_exact_state_and_path() {
        fn request(target: &str) -> Option<Result<String, String>> {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (mut server, _) = listener.accept().unwrap();
            write!(client, "GET {target} HTTP/1.1\r\n\r\n").unwrap();
            callback(&mut server, "expected")
        }
        assert!(request("/oauth-callback?state=other&code=attacker").is_none());
        assert!(request("/other?state=expected&code=attacker").is_none());
        assert_eq!(
            request("/oauth-callback?state=expected&code=hello%2Bworld")
                .unwrap()
                .unwrap(),
            "hello+world"
        );
        assert!(
            request("/oauth-callback?state=expected&error=access_denied")
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn authorization_uses_pkce_and_loopback_redirect() {
        let url = authorize_url("test-client", "state", "test-verifier");
        let query: std::collections::BTreeMap<_, _> = url.query_pairs().collect();
        assert_eq!(url.host_str(), Some("accounts.google.com"));
        assert_eq!(query["redirect_uri"], REDIRECT_URI);
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["state"], "state");
        assert_ne!(query["code_challenge"], "test-verifier");
        assert!(!query.contains_key("client_secret"));
    }
}
