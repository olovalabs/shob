//! `rpi auth` subcommand — persistent credential management. Mirrors the
//! Rust-relevant slice of the TS `packages/coding-agent/src/cli/auth-command.ts`
//! + `main.ts:runAuthCommand` + `core/auth-check.ts`.
//!
//! The host owns credential storage and standard login/check/logout/refresh
//! commands. Packages own provider-specific policy and OAuth implementations.
//!
//! - `rpi auth login`  — legacy API-key prompt (no echo, via `rpassword`) for
//!   the default provider and persist it to `~/.rpi/auth.json`.
//!   Mirrors the upstream `/login` TUI's `type:"secret"` prompt + `modify`.
//! - `rpi auth check`  — local-only probe of `auth.json`, models.json and the
//!   provider's declared policy; reports `ready`/`not_ready` (no network call — the upstream
//!   `--no-refresh` equivalent). `--json` emits a structured result.
//! - `rpi auth logout` — drop the selected provider entry from `auth.json`
//!   (env vars are left untouched).
//!
//! # Not ported (deferred — see the initial port notes (retired))
//!
//! Provider-specific OAuth implementations belong in extensions. The host only
//! understands the standard OAuth action response and scoped credential format.

use std::io::{self, Write};
use std::sync::Arc;

use crate::config::{
    self, delete_credential, read_auth, upsert_credential, Credential, DEFAULT_PROVIDER_ID,
};
use crate::credentials::CredentialSource;
use rpi_runtime::credentials::{oauth_needs_refresh, OAuthRefresher, RefreshedOAuth};
/// Exit code for a missing-credential `auth check` (mirrors upstream's
/// non-zero `auth check` when not ready).
const EXIT_NOT_READY: i32 = 1;
/// Exit code for an operational error (IO failure, bad args).
const EXIT_ERROR: i32 = 2;

/// `rpi auth <sub> [args]` entry. `args` is the slice *after* `auth` (i.e. the
/// subcommand + its flags). Returns the process exit code. Async to match the
/// `app::run` shape, though v1 does no async work here.
pub async fn run(args: &[String]) -> i32 {
    run_with_extensions(args, None).await
}

/// Run auth with an optional extension snapshot. The caller must keep the
/// matching extension session alive until this future completes.
pub async fn run_with_extensions(
    args: &[String],
    snapshot: Option<Arc<rpi_extensions::RegistrySnapshot>>,
) -> i32 {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("");
    match sub {
        "login" => run_login(&args[1..], snapshot.as_ref()).await,
        "check" | "status" => run_check(&args[1..], snapshot.as_ref()).await,
        "refresh" => run_refresh(&args[1..], snapshot.as_ref()).await,
        "logout" => run_logout(&args[1..], snapshot.as_ref()).await,
        "--help" | "-h" | "help" | "" => {
            print_auth_help();
            0
        }
        other => {
            eprintln!("error: unknown auth subcommand \"{other}\"");
            eprintln!();
            print_auth_help();
            EXIT_ERROR
        }
    }
}

/// `rpi auth login [--provider <id>]` — prompt for a key and persist it.
async fn run_login(
    args: &[String],
    snapshot: Option<&Arc<rpi_extensions::RegistrySnapshot>>,
) -> i32 {
    let provider = parse_provider(args).unwrap_or(DEFAULT_PROVIDER_ID);
    if let Some(snapshot) = snapshot {
        if snapshot.oauth_provider(provider).is_some() {
            return run_oauth_login(provider, snapshot).await;
        }
    }
    // Compatibility API-key login. This path is provider-neutral; OAuth
    // behavior is supplied by the matching extension above.
    eprint!("Enter API key for {provider}: ");
    let key = match rpassword::read_password() {
        Ok(k) => k,
        Err(e) => {
            eprintln!();
            eprintln!("error: could not read the key from the terminal: {e}");
            return EXIT_ERROR;
        }
    };
    eprintln!();
    let key = key.trim();
    if key.is_empty() {
        eprintln!("error: an empty key was entered; nothing saved.");
        return EXIT_ERROR;
    }
    let cred = Credential::ApiKey {
        key: Some(key.to_string()),
        env: None,
    };
    if let Err(e) = upsert_credential(provider, cred) {
        eprintln!("error: could not save credentials: {e}");
        return EXIT_ERROR;
    }
    let path = match config::auth_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("warn: credentials saved, but could not resolve the config path: {e}");
            return 0;
        }
    };
    println!("Credentials saved to {}", path.display());
    println!("Run `rpi auth check --provider {provider}` to verify.");
    0
}

/// `rpi auth check [--provider <id>] [--json]` — local readiness probe.
async fn run_check(
    args: &[String],
    snapshot: Option<&Arc<rpi_extensions::RegistrySnapshot>>,
) -> i32 {
    let provider = parse_provider(args).unwrap_or(DEFAULT_PROVIDER_ID);
    let want_json = args.iter().any(|a| a == "--json");

    let source = match detect_credential(provider, snapshot) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("error: could not read authentication configuration: {error}");
            return EXIT_ERROR;
        }
    };
    let ready = source.is_present();
    if want_json {
        let json = serde_json::json!({
            "ready": ready,
            "provider": provider,
            "source": source.as_json_str(),
        });
        println!("{json}");
    } else if ready {
        println!("ready ({})", source.as_json_str());
    } else {
        println!("not_ready — no credentials found.");
        eprintln!();
        eprintln!("Run `rpi auth login --provider {provider}`, configure this provider's apiKey in models.json, or pass --api-key when starting rpi.");
        let env = match snapshot.and_then(|snapshot| snapshot.provider_manifest(provider)) {
            Some(manifest) => manifest.api_key_env.as_deref(),
            None => crate::credentials::builtin_api_key_env(provider),
        };
        if let Some(env) = env {
            eprintln!("You can also set {env}.");
        }
    }
    if ready {
        0
    } else {
        EXIT_NOT_READY
    }
}

/// `rpi auth refresh [--provider <id>]` — refresh a stored OAuth credential.
async fn run_refresh(
    args: &[String],
    snapshot: Option<&Arc<rpi_extensions::RegistrySnapshot>>,
) -> i32 {
    let provider = parse_provider(args).unwrap_or(DEFAULT_PROVIDER_ID);
    let Some(snapshot) = snapshot else {
        eprintln!("error: no OAuth extensions are loaded");
        return EXIT_ERROR;
    };
    match refresh_stored_oauth(snapshot, provider, false) {
        Ok(_) => {
            println!("OAuth credentials refreshed for \"{provider}\".");
            0
        }
        Err(RefreshError::NotStored) => {
            eprintln!("error: no stored OAuth credentials for \"{provider}\"");
            EXIT_NOT_READY
        }
        Err(error) => {
            eprintln!("error: {error}");
            EXIT_ERROR
        }
    }
}

/// Refresh errors must never put provider output or credential JSON in a transcript.
enum RefreshError {
    NotStored,
    Read,
    Plugin(rpi_extensions::OAuthExtensionError),
    Invalid,
    Save,
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotStored => f.write_str("no stored OAuth credentials"),
            Self::Read => f.write_str("could not read stored OAuth credentials"),
            Self::Plugin(rpi_extensions::OAuthExtensionError::PluginStatus(status, _)) => {
                write!(f, "the provider package returned status {status}")
            }
            Self::Plugin(_) => f.write_str("the provider package could not refresh the login"),
            Self::Invalid => f.write_str("OAuth refresh returned an invalid or expired credential"),
            Self::Save => f.write_str("could not save refreshed OAuth credentials"),
        }
    }
}

/// Renewal lock for one provider id, created on first use. Renewals of the same
/// provider serialize on it, so the second waits and then re-reads the store
/// the first one saved. Renewals of different providers never share a lock.
fn renewal_lock(provider: &str) -> Arc<std::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Arc<std::sync::Mutex<()>>>>,
    > = std::sync::OnceLock::new();
    let locks = LOCKS.get_or_init(Default::default);
    let mut locks = locks
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(locks.entry(provider.to_owned()).or_default())
}

/// Refresh `provider`'s stored OAuth credential through its package and save
/// the result. Shared by `rpi auth refresh` and request-time renewal.
fn refresh_stored_oauth(
    snapshot: &Arc<rpi_extensions::RegistrySnapshot>,
    provider: &str,
    only_if_needed: bool,
) -> Result<RefreshedOAuth, RefreshError> {
    // Serialize renewal of this provider across independently resolved sessions
    // and manual refresh. The lock is held across the package callback, which may
    // hang, so it is per provider and never shared with another one. It serializes
    // one process only: auth.json has no cross-process lock, so two processes may
    // still both refresh, as two `rpi auth refresh` runs can.
    let renewal = renewal_lock(provider);
    let _renewing = renewal
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(Credential::Oauth {
        access,
        refresh,
        expires,
    }) = config::read_oauth_token(provider).map_err(|_| RefreshError::Read)?
    else {
        return Err(RefreshError::NotStored);
    };
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(i64::MAX, |duration| duration.as_secs() as i64)
    };
    // A different binding or process may have renewed the credential already.
    if only_if_needed && !access.trim().is_empty() && !oauth_needs_refresh(expires, now()) {
        return Ok(RefreshedOAuth { access, expires });
    }
    if refresh.trim().is_empty() {
        return Err(RefreshError::Invalid);
    }
    let started_from = (access.clone(), refresh.clone(), expires);
    let response = rpi_extensions::request_oauth(
        snapshot,
        provider,
        "refresh",
        serde_json::json!({"credential": {"access": access, "refresh": refresh, "expires_at": expires}}),
    )
    .map_err(RefreshError::Plugin)?;
    let response =
        rpi_extensions::OAuthActionResponse::parse(response).map_err(|_| RefreshError::Invalid)?;
    if response
        .payload
        .get("expires_at")
        .is_some_and(|value| value.as_i64().is_none())
        || response
            .payload
            .get("refresh")
            .is_some_and(|value| !value.is_string())
    {
        return Err(RefreshError::Invalid);
    }
    let token = response
        .credential()
        .map_err(|_| RefreshError::Invalid)?
        .ok_or(RefreshError::Invalid)?;
    if token.access.trim().is_empty() || (token.expires_at != 0 && token.expires_at <= now()) {
        return Err(RefreshError::Invalid);
    }
    // Providers need not rotate the refresh token on every renewal.
    let refresh = if token.refresh.trim().is_empty() {
        refresh
    } else {
        token.refresh
    };
    let committed = config::replace_oauth_token_if_unchanged(
        provider,
        (&started_from.0, &started_from.1, started_from.2),
        token.access.clone(),
        refresh,
        token.expires_at,
    )
    .map_err(|_| RefreshError::Save)?;
    if !committed {
        // Login and logout don't take the renewal lock, so they can land while
        // the package call runs. Keep what the user did and drop this result.
        return match config::read_oauth_token(provider).map_err(|_| RefreshError::Read)? {
            Some(Credential::Oauth {
                access, expires, ..
            }) => Ok(RefreshedOAuth { access, expires }),
            _ => Err(RefreshError::NotStored),
        };
    }
    Ok(RefreshedOAuth {
        access: token.access,
        expires: token.expires_at,
    })
}

/// Renews stored OAuth credentials through the loaded provider packages before
/// a request sends them.
pub(crate) struct PackageOAuthRefresher {
    snapshot: Arc<rpi_extensions::RegistrySnapshot>,
    _keepalive: Arc<rpi_extensions::PluginKeepalive>,
}

impl PackageOAuthRefresher {
    pub(crate) fn new(session: &rpi_extensions::ExtensionSession) -> Option<Self> {
        Some(Self {
            snapshot: session.snapshot_arc()?,
            _keepalive: session.keepalive(),
        })
    }
}

impl OAuthRefresher for PackageOAuthRefresher {
    fn supports(&self, provider: &str) -> bool {
        self.snapshot.oauth_provider(provider).is_some()
    }

    fn refresh(&self, provider: &str) -> Result<RefreshedOAuth, String> {
        refresh_stored_oauth(&self.snapshot, provider, true).map_err(|error| error.to_string())
    }
}

async fn run_logout(
    args: &[String],
    snapshot: Option<&Arc<rpi_extensions::RegistrySnapshot>>,
) -> i32 {
    let provider = parse_provider(args).unwrap_or(DEFAULT_PROVIDER_ID);
    if let Ok(Some(Credential::Oauth {
        access, refresh, ..
    })) = config::read_oauth_token(provider)
    {
        let Some(snapshot) = snapshot else {
            eprintln!("error: cannot revoke OAuth credentials: no OAuth extensions are loaded");
            return EXIT_ERROR;
        };
        let response = match rpi_extensions::request_oauth(
            snapshot,
            provider,
            "revoke",
            serde_json::json!({"credential": {"access": access, "refresh": refresh}}),
        ) {
            Ok(response) => response,
            Err(error) => {
                eprintln!("error: OAuth revoke failed; local credentials were kept: {error}");
                return EXIT_ERROR;
            }
        };
        match rpi_extensions::OAuthActionResponse::parse(response) {
            Ok(response) if response.kind == rpi_extensions::OAuthResponseKind::Success => {}
            Ok(_) => {
                eprintln!(
                    "error: OAuth revoke did not return success; local credentials were kept"
                );
                return EXIT_ERROR;
            }
            Err(error) => {
                eprintln!(
                    "error: invalid OAuth revoke response; local credentials were kept: {error}"
                );
                return EXIT_ERROR;
            }
        }
        /*
         */
    }
    match delete_credential(provider) {
        Ok(true) => {
            println!("Removed stored credentials for \"{provider}\".");
            0
        }
        Ok(false) => {
            println!("No stored credential for \"{provider}\" (nothing to do).");
            0
        }
        Err(e) => {
            eprintln!("error: could not remove credentials: {e}");
            EXIT_ERROR
        }
    }
}

/// Pull the `--provider <id>` value from a subcommand's args (defaults to
/// `None`). Mirrors the TS `--provider` handshake before it falls back to the
/// default.
async fn run_oauth_login(provider: &str, snapshot: &Arc<rpi_extensions::RegistrySnapshot>) -> i32 {
    let mut input = serde_json::json!({});
    let mut action = "begin";
    loop {
        let raw = match rpi_extensions::request_oauth(snapshot, provider, action, input) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("error: OAuth {action} failed: {error}");
                return EXIT_ERROR;
            }
        };
        let response = match rpi_extensions::OAuthActionResponse::parse(raw) {
            Ok(response) => response,
            Err(error) => {
                eprintln!("error: invalid OAuth response: {error}");
                return EXIT_ERROR;
            }
        };
        match response.kind {
            rpi_extensions::OAuthResponseKind::Credential => {
                let Some(token) = response.credential().ok().flatten() else {
                    eprintln!("error: OAuth response contained no valid credential");
                    return EXIT_ERROR;
                };
                if let Err(error) = config::upsert_oauth_token(
                    provider,
                    token.access,
                    token.refresh,
                    token.expires_at,
                ) {
                    eprintln!("error: could not save OAuth credentials: {error}");
                    return EXIT_ERROR;
                }
                println!("OAuth credentials saved for \"{provider}\".");
                return 0;
            }
            rpi_extensions::OAuthResponseKind::Error => {
                eprintln!("error: OAuth provider rejected the request");
                return EXIT_ERROR;
            }
            rpi_extensions::OAuthResponseKind::Success => {
                eprintln!("error: OAuth login completed without credentials");
                return EXIT_ERROR;
            }
            rpi_extensions::OAuthResponseKind::Interaction => {
                let display = response.payload.get("display").unwrap_or(&response.payload);
                if let Some(title) = display.get("title").and_then(serde_json::Value::as_str) {
                    println!("{title}");
                }
                if let Some(url) = display.get("url").and_then(serde_json::Value::as_str) {
                    println!("{url}");
                }
                if let Some(instructions) = display
                    .get("instructions")
                    .and_then(serde_json::Value::as_array)
                {
                    for instruction in instructions.iter().filter_map(serde_json::Value::as_str) {
                        println!("{instruction}");
                    }
                }
                eprint!("OAuth input: ");
                let _ = io::stderr().flush();
                let mut line = String::new();
                if io::stdin().read_line(&mut line).is_err() || line.trim().is_empty() {
                    eprintln!("error: empty OAuth input; nothing saved.");
                    return EXIT_ERROR;
                }
                input = serde_json::json!({"value": line.trim()});
                action = "exchange";
            }
        }
    }
}

fn parse_provider(args: &[String]) -> Option<&str> {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--provider" {
            if let Some(v) = iter.next() {
                return Some(v.as_str());
            }
        } else if let Some(rest) = a.strip_prefix("--provider=") {
            return Some(rest);
        }
    }
    None
}

/// Local readiness uses the same provider identity, precedence and package
/// policy as requests. It never exposes the resolved credential material.
fn detect_credential(
    provider: &str,
    snapshot: Option<&Arc<rpi_extensions::RegistrySnapshot>>,
) -> Result<CredentialSource, config::ConfigError> {
    let store = read_auth()?;
    let models = config::load_models_config()?;
    let manifests = snapshot.map_or_else(Vec::new, |snapshot| {
        snapshot
            .providers()
            .iter()
            .filter_map(|provider| snapshot.provider_manifest(&provider.provider_id).cloned())
            .collect()
    });
    Ok(crate::credentials::status(provider, &models, &store, &manifests).source)
}

/// `rpi auth --help`. Mirrors the TS auth-command usage but scoped to v1.
fn print_auth_help() {
    println!(
        "Usage: {name} auth <subcommand> [options]

Manage persisted credentials and inspect provider package authentication.

Subcommands:
  login   Prompt for an API key, or run an OAuth provider login.
  check   Report whether credentials are available (no network call).
  status  Alias for check; never prints token material.
  refresh Refresh an OAuth token through its provider extension.
  logout  Revoke OAuth credentials, then remove the stored credential.

Options:
  --provider <id>   Provider id (default: anthropic)
  --extension <path>  Load a provider or OAuth extension explicitly.
  --no-extensions     Disable extension discovery and package auth declarations.
  --json            (check only) Emit a {{ready, provider, source}} JSON object

Environment:
  ANTHROPIC_API_KEY      Fallback API key (x-api-key) when no stored credential.
  ANTHROPIC_AUTH_TOKEN   Fallback bearer token (Authorization: Bearer).
  OPENAI_API_KEY         Fallback bearer token for the built-in openai identity.
  Other providers       Environment names come from installed package declarations.

Notes:
  `login --provider <id>` uses the registered OAuth extension when one exists;
  otherwise it is a provider-neutral API-key compatibility prompt. The host
  only handles standard interaction and credential responses for OAuth.
  `refresh` and `logout` use extension actions. API-key providers may still
  use ~/.rpi/agent/models.json or environment variables.
",
        name = crate::APP_NAME
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        auth_path, test_support::env_lock, upsert_credential, Credential, DEFAULT_PROVIDER_ID,
    };

    /// Scope `RPI_CODING_AGENT_DIR` + the `ANTHROPIC_*` env vars to a temp dir
    /// for the duration of a test. Holds the shared env lock for its whole
    /// lifetime so it can't race with config/provider env-mutating tests.
    struct TempConfig {
        _guard: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        prev_dir: Option<std::ffi::OsString>,
        prev_key: Option<std::ffi::OsString>,
        prev_tok: Option<std::ffi::OsString>,
        prev_openai_key: Option<std::ffi::OsString>,
    }
    impl TempConfig {
        fn new() -> Self {
            let guard = env_lock().lock().unwrap();
            let prev_dir = std::env::var_os(crate::config::CONFIG_DIR_ENV);
            let prev_key = std::env::var_os(crate::provider::ANTHROPIC_API_KEY_ENV);
            let prev_tok = std::env::var_os(crate::provider::ANTHROPIC_AUTH_TOKEN_ENV);
            let prev_openai_key = std::env::var_os(crate::provider::OPENAI_API_KEY_ENV);
            let tmp = tempfile::TempDir::new().unwrap();
            std::env::set_var(crate::config::CONFIG_DIR_ENV, tmp.path());
            std::env::remove_var(crate::provider::ANTHROPIC_API_KEY_ENV);
            std::env::remove_var(crate::provider::ANTHROPIC_AUTH_TOKEN_ENV);
            std::env::remove_var(crate::provider::OPENAI_API_KEY_ENV);
            Self {
                _guard: guard,
                _tmp: tmp,
                prev_dir,
                prev_key,
                prev_tok,
                prev_openai_key,
            }
        }
    }
    impl Drop for TempConfig {
        fn drop(&mut self) {
            restore(crate::config::CONFIG_DIR_ENV, self.prev_dir.take());
            restore(crate::provider::ANTHROPIC_API_KEY_ENV, self.prev_key.take());
            restore(
                crate::provider::ANTHROPIC_AUTH_TOKEN_ENV,
                self.prev_tok.take(),
            );
            restore(
                crate::provider::OPENAI_API_KEY_ENV,
                self.prev_openai_key.take(),
            );
        }
    }
    fn restore(name: &str, prev: Option<std::ffi::OsString>) {
        match prev {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
    }

    #[test]
    fn standard_oauth_credential_response_is_parsed() {
        let response = rpi_extensions::OAuthActionResponse::parse(serde_json::json!({
            "version": 1,
            "kind": "credential",
            "payload": {
                "access": "access",
                "refresh": "refresh",
                "expires_at": 123
            }
        }))
        .unwrap();
        let credential = response.credential().unwrap().unwrap();
        assert_eq!(credential.access, "access");
        assert_eq!(credential.refresh, "refresh");
        assert_eq!(credential.expires_at, 123);
    }

    #[tokio::test]
    async fn check_not_ready_with_no_credentials() {
        let _cfg = TempConfig::new();
        let code = run_check(&[], None).await;
        assert_eq!(code, EXIT_NOT_READY);
    }

    #[tokio::test]
    async fn check_ready_with_stored_credential() {
        let _cfg = TempConfig::new();
        upsert_credential(
            DEFAULT_PROVIDER_ID,
            Credential::ApiKey {
                key: Some("sk-stored".into()),
                env: None,
            },
        )
        .unwrap();
        let code = run_check(&[], None).await;
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn check_ready_with_oauth_credential() {
        let _cfg = TempConfig::new();
        crate::config::upsert_oauth_token(
            DEFAULT_PROVIDER_ID,
            "access".into(),
            "refresh".into(),
            0,
        )
        .unwrap();
        assert_eq!(run_check(&[], None).await, 0);
    }

    #[tokio::test]
    async fn check_rejects_expired_oauth_credential() {
        let _cfg = TempConfig::new();
        crate::config::upsert_oauth_token(
            DEFAULT_PROVIDER_ID,
            "access".into(),
            "refresh".into(),
            1,
        )
        .unwrap();
        assert_eq!(run_check(&[], None).await, EXIT_NOT_READY);
    }

    #[tokio::test]
    async fn check_ready_with_env_api_key() {
        let _cfg = TempConfig::new();
        std::env::set_var(crate::provider::ANTHROPIC_API_KEY_ENV, "sk-env");
        let code = run_check(&[], None).await;
        assert_eq!(code, 0);
    }

    #[test]
    fn check_does_not_borrow_credentials_from_another_provider_or_protocol() {
        let _cfg = TempConfig::new();
        std::env::set_var(crate::provider::ANTHROPIC_API_KEY_ENV, "anthropic-key");
        std::env::set_var(crate::provider::OPENAI_API_KEY_ENV, "openai-key");
        std::fs::write(
            config::models_path().unwrap(),
            r#"{"providers":{
            "gateway":{"api":"openai-completions","models":[{"id":"model"}]},
            "another":{"api":"openai-completions","apiKey":"another-key","models":[{"id":"other"}]}
        }}"#,
        )
        .unwrap();
        for provider in ["gateway", "unknown", "openai-completions", "OpenAI"] {
            assert_eq!(
                detect_credential(provider, None).unwrap(),
                CredentialSource::Missing,
                "{provider}"
            );
        }
        // A gateway key also must not make the built-in OpenAI identity ready.
        std::env::remove_var(crate::provider::OPENAI_API_KEY_ENV);
        assert_eq!(
            detect_credential("openai", None).unwrap(),
            CredentialSource::Missing
        );
    }

    #[test]
    fn check_resolves_stored_references_and_rejects_empty_or_unresolved_values() {
        let _cfg = TempConfig::new();
        upsert_credential(
            "gateway",
            Credential::ApiKey {
                key: Some("$RPI_TEST_STORED_KEY".into()),
                env: Some(std::collections::BTreeMap::from([(
                    "RPI_TEST_STORED_KEY".into(),
                    "stored-key".into(),
                )])),
            },
        )
        .unwrap();
        assert_eq!(
            detect_credential("gateway", None).unwrap(),
            CredentialSource::StoredApiKey
        );
        for key in [Some("$RPI_TEST_AUTH_MISSING_VARIABLE"), Some("   "), None] {
            upsert_credential(
                "gateway",
                Credential::ApiKey {
                    key: key.map(str::to_string),
                    env: None,
                },
            )
            .unwrap();
            assert_eq!(
                detect_credential("gateway", None).unwrap(),
                CredentialSource::Missing
            );
        }
    }

    #[tokio::test]
    async fn check_reports_config_errors_and_respects_resolved_header_auth() {
        let _cfg = TempConfig::new();
        std::fs::write(
            config::models_path().unwrap(),
            r#"{"providers":{"gateway":{
            "api":"openai-completions","models":[{"id":"model","headers":{"Authorization":""}}]
        }}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_credential("gateway", None).unwrap(),
            CredentialSource::Missing
        );
        std::fs::write(config::models_path().unwrap(), r#"{"providers":{"gateway":{
            "api":"openai-completions","models":[{"id":"model","headers":{"Authorization":"Bearer key"}}]
        }}}"#).unwrap();
        assert_eq!(
            detect_credential("gateway", None).unwrap(),
            CredentialSource::Headers
        );
        std::fs::write(config::auth_path().unwrap(), "{invalid").unwrap();
        assert_eq!(run_check(&[], None).await, EXIT_ERROR);
    }

    #[tokio::test]
    async fn check_ready_with_openai_models_json_key() {
        let _cfg = TempConfig::new();
        std::fs::write(
            crate::config::models_path().unwrap(),
            r#"{"providers":{"gateway":{"api":"openai-completions","apiKey":"key","models":[{"id":"gpt-test"}]}}}"#,
        )
        .unwrap();
        let code = run_check(&["--provider".to_string(), "gateway".to_string()], None).await;
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn check_json_outputs_object() {
        let _cfg = TempConfig::new();
        // Capture stdout is awkward in unit tests; just assert the exit code +
        // that a stored cred flips `ready`.
        upsert_credential(
            DEFAULT_PROVIDER_ID,
            Credential::ApiKey {
                key: Some("sk-x".into()),
                env: None,
            },
        )
        .unwrap();
        let code = run_check(&["--json".to_string()], None).await;
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn logout_removes_stored_credential() {
        let _cfg = TempConfig::new();
        upsert_credential(
            DEFAULT_PROVIDER_ID,
            Credential::ApiKey {
                key: Some("sk".into()),
                env: None,
            },
        )
        .unwrap();
        assert!(auth_path().unwrap().exists());
        let code = run_logout(&[], None).await;
        assert_eq!(code, 0);
        // The entry should be gone → check is now not_ready.
        assert_eq!(run_check(&[], None).await, EXIT_NOT_READY);
    }

    #[tokio::test]
    async fn logout_when_empty_is_noop() {
        let _cfg = TempConfig::new();
        let code = run_logout(&[], None).await;
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn unknown_auth_subcommand_errors() {
        let code = run(&["bogus".to_string()]).await;
        assert_eq!(code, EXIT_ERROR);
    }

    #[tokio::test]
    async fn auth_help_exits_zero() {
        let code = run(&[]).await;
        assert_eq!(code, 0);
        let code = run(&["--help".to_string()]).await;
        assert_eq!(code, 0);
    }

    #[test]
    fn parse_provider_handles_both_forms() {
        assert_eq!(parse_provider(&[]), None);
        assert_eq!(
            parse_provider(&["--provider".to_string(), "anthropic".to_string()]),
            Some("anthropic")
        );
        assert_eq!(
            parse_provider(&["--provider=custom".to_string()]),
            Some("custom")
        );
        // A trailing flag with no value doesn't panic.
        assert_eq!(parse_provider(&["--provider".to_string()]), None);
    }
}
