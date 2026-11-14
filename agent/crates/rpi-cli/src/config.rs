//! # Layout
//!
//! Uses a nested layout: a `~/.rpi/agent/` directory (or one pointed at via
//! `RPI_CODING_AGENT_DIR`) "just works":
//!
//! ```text
//! ~/.rpi/                 (RPI_CODING_AGENT_DIR env overrides the agent/ dir)
//! └── agent/
//!     ├── auth.json       # persisted credentials (mode 0o600 on Unix)
//!     ├── models.json     # user-defined provider/model catalog (hand-edited)
//!     ├── settings.json   # saved default provider/model/thinking + theme
//!     ├── trust.json      # per-cwd project trust decisions (read-only parity)
//!     ├── .setup_done     # first-time-setup sentinel (extras.rs)
//! ```
//!
//! Flat-installed `~/.rpi/{auth.json,models.json}` from older rpi releases are
//! migrated under `agent/` on the next launch by [`migrate_legacy_layout`]
//! (best-effort, idempotent; only when the env override is unset).
//!
//! # Concurrency
//!
//! v1 is a single-process CLI, so we use **atomic rename** instead of upstream's
//! `proper-lockfile`: write a sibling temp file, `fs::rename` over the target,
//! then `chmod 0o600` on Unix (Windows chmod is a no-op, matching Node).
//! Within one process, auth.json read-modify-write updates (upsert and delete)
//! run under a short in-memory lock, so concurrent provider renewals cannot
//! drop each other's entries. Across processes, two `rpi auth login` runs from
//! two shells could still lose one update — that's accepted and documented;
//! adding a file lock is deferred.
//!
//! # Config-value expansion
//!
//! [`resolve_config_value`] expands `$ENV`/`${ENV}`/`!command` inside
//! `apiKey`/`headers` exactly like upstream's `resolve-config-value.ts` — so a
//! copied pi `models.json`/`auth.json` that references env vars or shell
//! commands resolves the same way. Applied where rpi consumes those values
//! (auth.json key, models.json bearer apiKey, provider/model `headers`).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use rpi_ai::Model;
#[cfg(test)]
use rpi_ai::{Api, ModelCost, StreamingProtocolCompat};
pub use rpi_runtime::config::{
    default_anthropic_base_url, model_compat_warnings, provider_is_anthropic_compatible,
    provider_is_openai_completions, provider_is_openai_responses, AuthStore, ConfigError,
    Credential, ModelDefinition, ModelsConfig, ProviderConfig, ANTHROPIC_DEFAULT_BASE_URL,
};

/// The config directory name under the home dir.
pub const CONFIG_DIR_NAME: &str = ".rpi";

/// Env var that overrides the whole config dir. Absolute path; relative
/// values are rejected.
pub const CONFIG_DIR_ENV: &str = "RPI_CODING_AGENT_DIR";

/// The provider id under which `rpi auth login` stores the Anthropic key.
/// Mirrors upstream's fixed `anthropic` provider id.
pub use rpi_runtime::config::DEFAULT_PROVIDER_ID;

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// The rpi config directory (`~/.rpi/agent` by default, `RPI_CODING_AGENT_DIR`
/// override). Creates nothing — purely a path computation. The `agent/` layer
/// is `join(homedir(), CONFIG_DIR_NAME, "agent")`. The env override points at
/// the agent dir itself.
pub fn agent_dir() -> Result<PathBuf, ConfigError> {
    if let Some(val) = std::env::var_os(CONFIG_DIR_ENV) {
        let p = PathBuf::from(&val);
        if !p.is_absolute() {
            return Err(ConfigError::RelativeOverride {
                env: CONFIG_DIR_ENV,
                val: val.to_string_lossy().into_owned(),
            });
        }
        return Ok(p);
    }
    let home = dirs::home_dir().ok_or(ConfigError::NoHomeDir {
        env: CONFIG_DIR_ENV,
    })?;
    Ok(home.join(CONFIG_DIR_NAME).join("agent"))
}

/// The config dir one level above the agent dir (`~/.rpi`, or the parent of an
/// env override). Used by [`migrate_legacy_layout`] to locate the old flat
/// layout. Returns `None` when the env override has no parent (a root path).
fn config_root_dir() -> Result<PathBuf, ConfigError> {
    let agent = agent_dir()?;
    agent
        .parent()
        .map(Path::to_path_buf)
        .ok_or(ConfigError::NoHomeDir {
            env: CONFIG_DIR_ENV,
        })
}

/// `~/.rpi/agent/auth.json`.
pub fn auth_path() -> Result<PathBuf, ConfigError> {
    Ok(agent_dir()?.join("auth.json"))
}

/// `~/.rpi/agent/models.json`.
pub fn models_path() -> Result<PathBuf, ConfigError> {
    Ok(agent_dir()?.join("models.json"))
}

/// `~/.rpi/agent/settings.json` (saved default provider/model/thinking + theme).
pub fn settings_path() -> Result<PathBuf, ConfigError> {
    Ok(agent_dir()?.join("settings.json"))
}

/// `~/.rpi/agent/trust.json` (per-cwd project trust decisions).
pub fn trust_path() -> Result<PathBuf, ConfigError> {
    Ok(agent_dir()?.join("trust.json"))
}

fn read_text_with_fallback(
    primary: PathBuf,
    fallback: Option<PathBuf>,
) -> Result<Option<(PathBuf, String)>, ConfigError> {
    match std::fs::read_to_string(&primary) {
        Ok(text) => Ok(Some((primary, text))),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            let Some(fallback) = fallback.filter(|path| path != &primary) else {
                return Ok(None);
            };
            match std::fs::read_to_string(&fallback) {
                Ok(text) => Ok(Some((fallback, text))),
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(source) => Err(ConfigError::Read {
                    path: fallback,
                    source,
                }),
            }
        }
        Err(source) => Err(ConfigError::Read {
            path: primary,
            source,
        }),
    }
}

/// One-time best-effort migration of a pre-nesting flat layout
/// (`~/.rpi/{auth.json,models.json,.setup_done}`) into the
/// nested `~/.rpi/agent/` layout. **No-op when `RPI_CODING_AGENT_DIR` is set**
/// (never touch an explicit override), when the agent dir already exists, or
/// when no flat files are present. Idempotent: a partial move resumes. Errors
/// are swallowed (logged via the returned `Result` only so tests can observe);
/// `app::run` ignores them so a migration hiccup never blocks startup.
pub fn migrate_legacy_layout() -> Result<usize, ConfigError> {
    // Only migrate the default home-backed layout — never an env override.
    if std::env::var_os(CONFIG_DIR_ENV).is_some() {
        return Ok(0);
    }
    let root = match config_root_dir() {
        Ok(p) => p,
        Err(_) => return Ok(0),
    };
    let agent = agent_dir()?;
    migrate_legacy_layout_in(&root, &agent)
}

/// The core migration (no env gate): if `agent/` is absent but flat files exist
/// under `root`, move `{auth.json,models.json,.setup_done}` into
/// `agent/`. Idempotent. Factored out so tests can drive it against a temp
/// root/agent pair without touching the env (the public
/// [`migrate_legacy_layout`] short-circuits on an env override, which tests
/// can't unset portably while other tests run).
fn migrate_legacy_layout_in(root: &Path, agent: &Path) -> Result<usize, ConfigError> {
    // If the agent dir already exists with any content, assume already migrated.
    if agent.exists() {
        return Ok(0);
    }
    // Probe for a flat file. If none, nothing to migrate.
    let flat_auth = root.join("auth.json");
    let flat_models = root.join("models.json");
    if !flat_auth.exists() && !flat_models.exists() {
        return Ok(0);
    }
    std::fs::create_dir_all(agent).map_err(|e| ConfigError::Write {
        path: agent.to_path_buf(),
        source: e,
    })?;
    let mut moved = 0usize;
    for leaf in ["auth.json", "models.json", ".setup_done"] {
        let from = root.join(leaf);
        let to = agent.join(leaf);
        if from.exists() && !to.exists() {
            // `rename` across the same filesystem is atomic; fall back to copy
            // + remove on cross-device (rare for a home dir).
            if let Err(_e) = std::fs::rename(&from, &to) {
                if std::fs::copy(&from, &to).is_ok() {
                    let _ = std::fs::remove_file(&from);
                }
            }
            moved += 1;
        }
    }
    Ok(moved)
}

// ---------------------------------------------------------------------------
// auth.json — Credential store
// ---------------------------------------------------------------------------

/// Read the auth store. Missing file ⇒ empty store (not an error). Malformed
/// JSON ⇒ `ConfigError::Json` (we do not silently swallow a corrupt auth file).
pub fn read_auth() -> Result<AuthStore, ConfigError> {
    let path = auth_path()?;
    match read_text_with_fallback(path, None)? {
        Some((path, text)) => {
            serde_json::from_str(&text).map_err(|source| ConfigError::Json { path, source })
        }
        None => Ok(AuthStore::new()),
    }
}

/// Serializes this process's read-modify-write updates of the auth store. It is
/// held only for the read and the write, never while calling a provider
/// package, so two providers committing at once cannot drop each other's entry.
fn auth_store_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &LOCK
}

/// Atomically write the whole auth store (ensures the dir exists, writes a
/// temp sibling, `rename`s over the target, then `chmod 0o600` on Unix).
/// Callers that read-modify-write must hold the internal store lock.
pub fn write_auth(store: &AuthStore) -> Result<(), ConfigError> {
    let path = auth_path()?;
    let dir = agent_dir()?;
    ensure_dir(&dir)?;
    let json = serde_json::to_string_pretty(store).unwrap();
    atomic_write(&path, json.as_bytes())?;
    set_owner_only(&path);
    Ok(())
}

/// Read-modify-write: upsert a credential for `provider_id`. Re-reads the
/// latest store under the lock, so other providers' entries are preserved.
pub fn upsert_credential(provider_id: &str, cred: Credential) -> Result<(), ConfigError> {
    let _store_guard = auth_store_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut store = read_auth()?;
    store.insert(provider_id.to_string(), cred);
    write_auth(&store)
}

/// Store an OAuth token under exactly one provider id using the atomic auth
/// store writer. Token values never enter the event or session layer.
pub fn upsert_oauth_token(
    provider_id: &str,
    access: String,
    refresh: String,
    expires: i64,
) -> Result<(), ConfigError> {
    upsert_credential(
        provider_id,
        Credential::Oauth {
            access,
            refresh,
            expires,
        },
    )
}

/// Replace `provider_id`'s OAuth token only if the stored one still has the
/// `expected` access, refresh and expiry. Returns `false`, writing nothing, when
/// a login or logout replaced or removed it meanwhile, so a late refresh result
/// never undoes that user action.
pub fn replace_oauth_token_if_unchanged(
    provider_id: &str,
    expected: (&str, &str, i64),
    access: String,
    refresh: String,
    expires: i64,
) -> Result<bool, ConfigError> {
    let _store_guard = auth_store_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut store = read_auth()?;
    let unchanged = matches!(
        store.get(provider_id),
        Some(Credential::Oauth { access, refresh, expires })
            if (access.as_str(), refresh.as_str(), *expires) == expected
    );
    if unchanged {
        store.insert(
            provider_id.to_string(),
            Credential::Oauth {
                access,
                refresh,
                expires,
            },
        );
        write_auth(&store)?;
    }
    Ok(unchanged)
}

/// Read only the OAuth credential for one provider.
pub fn read_oauth_token(provider_id: &str) -> Result<Option<Credential>, ConfigError> {
    Ok(match read_auth()?.get(provider_id) {
        Some(Credential::Oauth {
            access,
            refresh,
            expires,
        }) => Some(Credential::Oauth {
            access: access.clone(),
            refresh: refresh.clone(),
            expires: *expires,
        }),
        _ => None,
    })
}

/// Remove `provider_id` from the store. Returns `true` if a credential was
/// present (and is now gone), `false` if it was already absent. Always rewrites
/// the file when the provider existed (so `auth logout` reflects the new state
/// on disk even if the map isn't empty).
pub fn delete_credential(provider_id: &str) -> Result<bool, ConfigError> {
    let _store_guard = auth_store_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut store = read_auth()?;
    if store.remove(provider_id).is_some() {
        write_auth(&store)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// models.json — provider/model catalog
// ---------------------------------------------------------------------------

/// Load `~/.rpi/models.json`. Missing file ⇒ empty config (no error).
pub fn load_models_config() -> Result<ModelsConfig, ConfigError> {
    let path = models_path()?;
    match read_text_with_fallback(path, None)? {
        Some((path, text)) => {
            parse_models_json(&text).map_err(|source| ConfigError::Json { path, source })
        }
        None => Ok(ModelsConfig::default()),
    }
}

/// Diagnose unsupported compat keys and malformed values without rejecting
/// unrelated providers.
/// Atomically write the whole `~/.rpi/models.json` (used by the interactive
/// provider form). Note: unlike [`crate::settings::save_settings`], this
/// serializes the modeled config directly, so hand-written comments and unknown
/// fields in `models.json` are not preserved on rewrite. A timestamped
/// `models.json.bak-*` snapshot is written first so a mistake can be reverted.
pub fn save_models_config(cfg: &ModelsConfig) -> Result<(), ConfigError> {
    let path = models_path()?;
    let dir = agent_dir()?;
    ensure_dir(&dir)?;
    // Back up the existing file before rewriting it (best-effort; a missing
    // file is normal on first save).
    if let Ok(existing) = std::fs::read(&path) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let backup = dir.join(format!("models.json.bak-{stamp}"));
        let _ = std::fs::write(&backup, existing);
    }
    let text = serde_json::to_string_pretty(cfg).map_err(|source| ConfigError::Json {
        path: path.clone(),
        source,
    })?;
    atomic_write(&path, text.as_bytes())
}

/// Add one user-configured provider and its first model to `models.json`.
///
/// This is shared by the interactive provider form and RPC clients so both
/// surfaces enforce the same protocol, URL, duplicate, and size checks.
pub fn add_provider(
    provider_id: &str,
    api: &str,
    base_url: &str,
    api_key: &str,
    model_id: &str,
    model_name: Option<&str>,
    context_window: Option<u64>,
    max_tokens: Option<u64>,
    supports_images: Option<bool>,
) -> Result<(), String> {
    const MAX_FIELD: usize = 4096;
    let provider_id = provider_id.trim();
    let api = api.trim();
    let base_url = base_url.trim();
    let api_key = api_key.trim();
    let model_id = model_id.trim();
    let model_name = model_name.map(str::trim).filter(|value| !value.is_empty());
    if provider_id.is_empty()
        || api.is_empty()
        || base_url.is_empty()
        || api_key.is_empty()
        || model_id.is_empty()
    {
        return Err("provider, protocol, base URL, API key, and model ID are required".into());
    }
    for (label, value) in [
        ("provider", provider_id),
        ("protocol", api),
        ("base URL", base_url),
        ("API key", api_key),
        ("model ID", model_id),
    ] {
        if value.len() > MAX_FIELD {
            return Err(format!("{label} is too long"));
        }
    }
    if model_name.is_some_and(|value| value.len() > MAX_FIELD) {
        return Err("model name is too long".into());
    }
    if context_window == Some(0) {
        return Err("context window must be greater than zero".into());
    }
    if max_tokens == Some(0) {
        return Err("max output tokens must be greater than zero".into());
    }
    if !matches!(
        api,
        "anthropic-messages" | "openai-completions" | "openai-responses"
    ) {
        return Err("unsupported provider protocol".into());
    }
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err("base URL must start with http:// or https://".into());
    }
    let mut config = load_models_config().map_err(|error| error.to_string())?;
    if config.providers.contains_key(provider_id) {
        return Err(format!(
            "provider {provider_id:?} already has a models.json configuration"
        ));
    }
    config.providers.insert(
        provider_id.to_owned(),
        ProviderConfig {
            name: None,
            base_url: Some(base_url.to_owned()),
            api_key: Some(api_key.to_owned()),
            api: Some(api.to_owned()),
            headers: None,
            auth_header: None,
            models: vec![ModelDefinition {
                id: model_id.to_owned(),
                name: model_name.map(str::to_owned),
                base_url: None,
                reasoning: None,
                thinking_level_map: None,
                context_window,
                max_tokens,
                cost: None,
                input: supports_images.map(|supports| {
                    if supports {
                        vec!["text".to_string(), "image".to_string()]
                    } else {
                        vec!["text".to_string()]
                    }
                }),
                headers: None,
                compat: None,
            }],
        },
    );
    save_models_config(&config).map_err(|error| error.to_string())
}

/// Update an existing models.json provider and its first model. An omitted API
/// key preserves the existing key, so editing metadata does not require
/// exposing or re-entering a secret.
pub fn update_provider(
    provider_id: &str,
    api: &str,
    base_url: &str,
    api_key: Option<&str>,
    model_id: &str,
    model_name: Option<&str>,
    context_window: Option<u64>,
    max_tokens: Option<u64>,
    supports_images: bool,
) -> Result<(), String> {
    let provider_id = provider_id.trim();
    let api = api.trim();
    let base_url = base_url.trim();
    let model_id = model_id.trim();
    let model_name = model_name.map(str::trim).filter(|value| !value.is_empty());
    if provider_id.is_empty() || api.is_empty() || base_url.is_empty() || model_id.is_empty() {
        return Err("provider, protocol, base URL, and model ID are required".into());
    }
    if !matches!(
        api,
        "anthropic-messages" | "openai-completions" | "openai-responses"
    ) {
        return Err("unsupported provider protocol".into());
    }
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        return Err("base URL must start with http:// or https://".into());
    }
    if context_window == Some(0) {
        return Err("context window must be greater than zero".into());
    }
    if max_tokens == Some(0) {
        return Err("max output tokens must be greater than zero".into());
    }
    let mut config = load_models_config().map_err(|error| error.to_string())?;
    let provider = config
        .providers
        .get_mut(provider_id)
        .ok_or_else(|| format!("provider {provider_id:?} has no models.json configuration"))?;
    provider.api = Some(api.to_owned());
    provider.base_url = Some(base_url.to_owned());
    if let Some(api_key) = api_key.map(str::trim).filter(|value| !value.is_empty()) {
        provider.api_key = Some(api_key.to_owned());
    }
    let model = provider
        .models
        .first_mut()
        .ok_or_else(|| format!("provider {provider_id:?} has no model to update"))?;
    model.id = model_id.to_owned();
    model.name = model_name.map(str::to_owned);
    model.context_window = context_window;
    model.max_tokens = max_tokens;
    model.input = Some(if supports_images {
        vec!["text".to_string(), "image".to_string()]
    } else {
        vec!["text".to_string()]
    });
    save_models_config(&config).map_err(|error| error.to_string())
}

/// Delete a user-configured provider from models.json.
pub fn delete_provider(provider_id: &str) -> Result<(), String> {
    let provider_id = provider_id.trim();
    if provider_id.is_empty() {
        return Err("provider is required".into());
    }
    let mut config = load_models_config().map_err(|error| error.to_string())?;
    if config.providers.shift_remove(provider_id).is_none() {
        return Err(format!(
            "provider {provider_id:?} has no models.json configuration"
        ));
    }
    save_models_config(&config).map_err(|error| error.to_string())
}

// ---------------------------------------------------------------------------
// trust.json — project-trust store (read-only layout parity with pi)
// ---------------------------------------------------------------------------

/// The trust store: `canonicalCwd -> decision` (`true`/`false`/`null`). Mirrors
/// pi's `TrustFile = Record<string, boolean | null | undefined>`
/// (`trust-manager.ts`). rpi reads this for layout parity (a copied pi
/// `trust.json` parses + is located correctly) but does **not** gate any
/// project resources behind trust in v1 — there is no trust prompt. Deferred.
pub type TrustStore = BTreeMap<String, Option<bool>>;

/// Read `~/.rpi/agent/trust.json`. Missing file ⇒ empty store (not an error).
/// Malformed JSON ⇒ `ConfigError::Json`. `null` decisions deserialize as
/// `None`; absent entries are simply not present.
pub fn read_trust() -> Result<TrustStore, ConfigError> {
    let path = trust_path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| ConfigError::Json {
            path: path.clone(),
            source: e,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TrustStore::new()),
        Err(e) => Err(ConfigError::Read { path, source: e }),
    }
}

/// Persist the trust decision for a project directory. The path is canonical
/// when it exists, with an absolute fallback for a project being created.
pub fn set_project_trust(cwd: &Path, trusted: Option<bool>) -> Result<(), ConfigError> {
    let key = std::fs::canonicalize(cwd)
        .unwrap_or_else(|_| cwd.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let mut store = read_trust()?;
    if let Some(decision) = trusted {
        store.insert(key, Some(decision));
    } else {
        store.remove(&key);
    }
    let path = trust_path()?;
    ensure_dir(&agent_dir()?)?;
    let json = serde_json::to_string_pretty(&store).unwrap();
    atomic_write(&path, json.as_bytes())?;
    set_owner_only(&path);
    Ok(())
}

/// Return the stored trust decision for `cwd`. A missing entry (or an entry
/// explicitly set to `null`) returns `None`; callers choose their safe default.
pub fn project_trust_decision(cwd: &Path) -> Result<Option<bool>, ConfigError> {
    let key = std::fs::canonicalize(cwd)
        .unwrap_or_else(|_| cwd.to_path_buf())
        .to_string_lossy()
        .into_owned();
    Ok(read_trust()?.get(&key).copied().flatten())
}

/// Parse the models JSON, tolerating `//` line comments (a minimal subset of
/// upstream's `stripJsonComments`). Tries strict JSON first; on failure, strips
/// `//…` to end-of-line and retries.
fn parse_models_json(text: &str) -> Result<ModelsConfig, serde_json::Error> {
    match serde_json::from_str(text) {
        Ok(c) => Ok(c),
        Err(first) => {
            // Best-effort comment strip — only `//` to EOL, never inside strings
            // (a `//` inside a JSON string would already have made the strict
            // parse fail for a *different* reason; stripping naively is an
            // acceptable v1 trade-off, documented as a limitation).
            let stripped = strip_line_comments(text);
            serde_json::from_str(&stripped).map_err(|_| first)
        }
    }
}

/// Strip `//` line comments (to end-of-line), skipping `//` that appears inside
/// a double-quoted string. A minimal subset of upstream's `stripJsonComments`,
/// shared by [`parse_models_json`] and [`crate::settings::load_settings`] so a
/// copied pi `models.json`/`settings.json` (which pi allows comments in) parses.
pub(crate) fn strip_line_comments(text: &str) -> String {
    text.lines()
        .map(|line| match find_line_comment(line) {
            Some(idx) => line[..idx].to_string(),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Index of a `//` line comment that is *not* inside a double-quoted string.
fn find_line_comment(line: &str) -> Option<usize> {
    let mut in_str = false;
    let mut esc = false;
    for (i, ch) in line.char_indices() {
        if esc {
            esc = false;
            continue;
        }
        match ch {
            '\\' if in_str => esc = true,
            '"' => in_str = !in_str,
            '/' if !in_str && line.as_bytes().get(i + 1) == Some(&b'/') => {
                return Some(i);
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Config-value expansion (mirrors pi `resolve-config-value.ts`)
// ---------------------------------------------------------------------------

/// A process-lifetime cache for `!command` resolutions, mirroring pi's
/// `commandResultCache`. Keyed by the raw `!cmd` string (including the `!`).
fn command_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, Option<String>>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, Option<String>>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Resolve a config value (API key, header value) that may be a shell command,
/// an env-var template, or a literal — mirroring pi's `resolveConfigValue`.
///
/// - `!command` → run the rest as a shell command (`sh -c` on Unix, `cmd /C` on
///   Windows), return trimmed stdout (cached per process). Missing shell or
///   non-zero exit ⇒ `None`.
/// - `$VAR` / `${VAR}` templates: interpolate from `env_overlay` (winning) then
///   the process env. `$$`→`$`, `$!`→`!` escapes. Any referenced var that is
///   unset ⇒ the **whole** value resolves to `None` (pi semantics).
/// - Otherwise the literal string (returned as-is).
///
/// `env_overlay` is the `credential.env` map for auth.json keys (pi passes the
/// same). `None` (or an empty overlay) means process-env only — used for
/// models.json apiKey/headers, which have no env overlay.
pub fn resolve_config_value(
    config: &str,
    env_overlay: Option<&BTreeMap<String, String>>,
) -> Option<String> {
    rpi_runtime::values::resolve_config_value(
        config,
        env_overlay,
        &crate::provider::CliValues,
        true,
    )
}

/// Like [`resolve_config_value`] but **uncached** — mirrors pi's
/// `resolveConfigValueUncached`, used when a fresh resolution is required
/// (e.g. headers, which pi resolves uncached so a rotating token is re-read).
pub fn resolve_config_value_uncached(
    config: &str,
    env_overlay: Option<&BTreeMap<String, String>>,
) -> Option<String> {
    rpi_runtime::values::resolve_config_value(
        config,
        env_overlay,
        &crate::provider::CliValues,
        false,
    )
}

/// Resolve every header value via [`resolve_config_value_uncached`]; drop
/// entries that resolve to `None` (mirrors pi `resolveHeaders`). Used on
/// models.json `headers` maps before folding onto a model.
pub fn resolve_headers(
    headers: &BTreeMap<String, String>,
    env_overlay: Option<&BTreeMap<String, String>>,
) -> BTreeMap<String, String> {
    rpi_runtime::values::resolve_headers(headers, env_overlay, &crate::provider::CliValues)
}

/// Run `cmd` (without the leading `!`), returning trimmed stdout. Cached per
/// process (mirrors pi `executeCommand`). 10s timeout; non-zero exit / missing
/// shell ⇒ `None`.
pub(crate) fn resolve_command(cmd: &str) -> Option<String> {
    let key = format!("!{cmd}");
    if let Some(v) = command_cache().lock().ok()?.get(&key) {
        return v.clone();
    }
    let result = resolve_command_uncached(cmd);
    if let Ok(mut cache) = command_cache().lock() {
        cache.insert(key, result.clone());
    }
    result
}

#[cfg(unix)]
fn spawn_shell_command(cmd: &str) -> Option<std::process::Output> {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
}

#[cfg(windows)]
fn spawn_shell_command(cmd: &str) -> Option<std::process::Output> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("cmd")
        .arg("/C")
        .arg(cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .ok()
}

/// Uncached `!command` execution (mirrors pi `executeCommandUncached`).
pub(crate) fn resolve_command_uncached(cmd: &str) -> Option<String> {
    let output = spawn_shell_command(cmd)?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// (`anthropic-messages`, or omitted/unknown). Unknown `api` is allowed through
/// for forward-compat but flagged ignored-in-v1 in the docs. Public so
/// [`crate::provider`] can scan models.json providers for an `authHeader:true`
/// gateway bearer source.
/// Resolve an OpenAI-compatible provider key from its explicit config or a
/// known provider's canonical environment variable. Arbitrary custom provider
/// ids must declare `apiKey: "$ENV"`: deriving an env name by normalizing the
/// id would collapse distinct native-Pi identities such as `a-b` and `a_b`.
pub fn openai_provider_api_key(provider_id: &str, cfg: &ProviderConfig) -> Option<String> {
    if let Some(raw) = cfg.api_key.as_deref().filter(|key| !key.is_empty()) {
        if let Some(value) = resolve_config_value(raw, None).filter(|value| !value.is_empty()) {
            return Some(value);
        }
    }

    let name = native_provider_api_key_env_var(provider_id)?;
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// Provider-id-to-environment-variable mapping. Provider ids are
/// identities, so aliases and case variants must not inherit another
/// provider's credential.
fn native_provider_api_key_env_var(provider_id: &str) -> Option<&'static str> {
    match provider_id {
        "openai" => crate::credentials::builtin_api_key_env(provider_id),
        _ => None,
    }
}

/// Convert a `(provider_id, ProviderConfig)` pair into a list of library
/// [`Model`]s. Provider-level `base_url`/`headers`/`auth_header` fold into each
/// model. Returns `None` for protocols that do not have a runtime provider.
// ---------------------------------------------------------------------------
// Internals: dir ensure, atomic write, chmod
// ---------------------------------------------------------------------------

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

/// Create the config dir if missing. Mode 0o700 on Unix (mkdir default on
/// Windows, where the sticky-permission concept doesn't apply).
fn ensure_dir(dir: &Path) -> Result<(), ConfigError> {
    if dir.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).map_err(|e| ConfigError::Write {
        path: dir.to_path_buf(),
        source: e,
    })?;
    #[cfg(unix)]
    {
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// Write `bytes` to `path` atomically: a temp sibling → `rename`. The temp
/// file lives next to the target so the rename stays on one filesystem.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ConfigError> {
    let dir = path.parent().ok_or_else(|| ConfigError::Write {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent"),
    })?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("rpi"),
        uuid::Uuid::new_v4().simple()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&tmp).map_err(|source| ConfigError::Write {
        path: tmp.clone(),
        source,
    })?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        // A partial staging file is never useful to a later caller. Best
        // effort cleanup also avoids leaving a misleading stale temp behind
        // when the target itself was left untouched.
        let _ = std::fs::remove_file(&tmp);
        return Err(ConfigError::Write {
            path: tmp,
            source: error,
        });
    }
    drop(file);
    if let Err(error) = std::fs::rename(&tmp, path) {
        // `rename` is the publish point; on failure the original target is
        // still intact. Remove the staging file before returning the error.
        let _ = std::fs::remove_file(&tmp);
        return Err(ConfigError::Write {
            path: path.to_path_buf(),
            source: error,
        });
    }
    Ok(())
}

/// Best-effort tighten to owner-only (0o600). No-op on Windows (the Node
/// upstream applies no ACL either).
fn set_owner_only(_path: &Path) {
    #[cfg(unix)]
    {
        let _ = std::fs::set_permissions(_path, std::fs::Permissions::from_mode(0o600));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_support {
    /// A shared workspace lock for tests that touch process-global env vars
    /// (`RPI_CODING_AGENT_DIR`, `ANTHROPIC_*`). All env-mutating tests across
    /// the crate (config / provider / auth) share this ONE mutex so they can't
    /// race on the shared environment. Hold the returned guard for the whole
    /// test (store it in a RAII struct).
    use std::sync::{Mutex, OnceLock};
    pub(crate) fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// Isolate session-directory overrides while holding the shared env lock.
    /// Restore the inherited value before releasing the lock, even on unwind.
    pub(crate) struct SessionDirEnv {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
    }

    impl SessionDirEnv {
        pub(crate) fn new(directory: Option<&std::path::Path>) -> Self {
            let guard = env_lock().lock().unwrap();
            let previous = std::env::var_os("RPI_CODING_AGENT_SESSION_DIR");
            match directory {
                Some(directory) => std::env::set_var("RPI_CODING_AGENT_SESSION_DIR", directory),
                None => std::env::remove_var("RPI_CODING_AGENT_SESSION_DIR"),
            }
            Self {
                _guard: guard,
                previous,
            }
        }
    }

    impl Drop for SessionDirEnv {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("RPI_CODING_AGENT_SESSION_DIR", value),
                None => std::env::remove_var("RPI_CODING_AGENT_SESSION_DIR"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_support::env_lock;

    /// Point `RPI_CODING_AGENT_DIR` at a fresh temp dir for the duration of the
    /// test (cleaned up on drop). Holds the prior value of the env var to
    /// restore it. Hold the env lock for its whole lifetime.
    struct TempConfig {
        _guard: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        prev: Option<std::ffi::OsString>,
    }
    impl TempConfig {
        fn new() -> Self {
            let guard = env_lock().lock().unwrap();
            let prev = std::env::var_os(CONFIG_DIR_ENV);
            let tmp = tempfile::TempDir::new().unwrap();
            std::env::set_var(CONFIG_DIR_ENV, tmp.path());
            Self {
                _guard: guard,
                _tmp: tmp,
                prev,
            }
        }
    }
    impl Drop for TempConfig {
        fn drop(&mut self) {
            restore_env(CONFIG_DIR_ENV, self.prev.take());
        }
    }

    #[test]
    fn read_auth_missing_file_is_empty() {
        let _cfg = TempConfig::new();
        let store = read_auth().unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn upsert_then_read_roundtrip() {
        let _cfg = TempConfig::new();
        upsert_credential(
            "anthropic",
            Credential::ApiKey {
                key: Some("sk-test-123".into()),
                env: None,
            },
        )
        .unwrap();
        let store = read_auth().unwrap();
        match store.get("anthropic") {
            Some(Credential::ApiKey { key, .. }) => assert_eq!(key.as_deref(), Some("sk-test-123")),
            other => panic!("unexpected cred: {other:?}"),
        }
        // The file should exist and be JSON.
        let path = auth_path().unwrap();
        assert!(path.exists(), "auth.json should exist after upsert");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"anthropic\""));
        assert!(raw.contains("api_key"));
    }

    #[test]
    fn add_provider_writes_protocol_endpoint_key_and_model() {
        let _cfg = TempConfig::new();
        add_provider(
            "gateway",
            "openai-completions",
            "https://api.example.test/v1",
            "fixture-key",
            "model-a",
            Some("Model A"),
            Some(128_000),
            Some(4096),
            Some(true),
        )
        .unwrap();
        let config = load_models_config().unwrap();
        let provider = config.providers.get("gateway").unwrap();
        assert_eq!(provider.api.as_deref(), Some("openai-completions"));
        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://api.example.test/v1")
        );
        assert_eq!(provider.api_key.as_deref(), Some("fixture-key"));
        assert_eq!(provider.models[0].id, "model-a");
        assert_eq!(provider.models[0].name.as_deref(), Some("Model A"));
        assert_eq!(provider.models[0].context_window, Some(128_000));
        assert_eq!(provider.models[0].max_tokens, Some(4096));
        assert_eq!(
            provider.models[0].input.as_deref(),
            Some(["text".to_string(), "image".to_string()].as_slice())
        );
    }

    #[test]
    fn add_provider_rejects_duplicate_and_unsupported_protocol() {
        let _cfg = TempConfig::new();
        add_provider(
            "gateway",
            "openai-responses",
            "https://api.example.test/v1",
            "fixture-key",
            "model-a",
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let duplicate = add_provider(
            "gateway",
            "openai-completions",
            "https://api.example.test/v1",
            "fixture-key-2",
            "model-b",
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(duplicate.contains("already"));
        let unsupported = add_provider(
            "other",
            "unsupported",
            "https://api.example.test/v1",
            "fixture-key",
            "model-a",
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(unsupported.contains("unsupported"));
        let invalid_context = add_provider(
            "context-zero",
            "openai-completions",
            "https://api.example.test/v1",
            "fixture-key",
            "model-a",
            None,
            Some(0),
            None,
            None,
        )
        .unwrap_err();
        assert!(invalid_context.contains("context window"));
    }

    #[test]
    fn update_and_delete_provider_change_models_json() {
        let _cfg = TempConfig::new();
        add_provider(
            "gateway",
            "openai-completions",
            "https://api.example.test/v1",
            "fixture-key",
            "model-a",
            Some("Model A"),
            Some(128_000),
            Some(4096),
            Some(false),
        )
        .unwrap();
        update_provider(
            "gateway",
            "openai-responses",
            "https://new.example.test/v1",
            None,
            "model-b",
            Some("Model B"),
            Some(256_000),
            Some(8192),
            true,
        )
        .unwrap();
        let provider = load_models_config()
            .unwrap()
            .providers
            .get("gateway")
            .unwrap()
            .clone();
        assert_eq!(provider.api.as_deref(), Some("openai-responses"));
        assert_eq!(
            provider.base_url.as_deref(),
            Some("https://new.example.test/v1")
        );
        assert_eq!(provider.api_key.as_deref(), Some("fixture-key"));
        assert_eq!(provider.models[0].id, "model-b");
        assert_eq!(provider.models[0].max_tokens, Some(8192));
        assert_eq!(
            provider.models[0].input.as_deref(),
            Some(["text".to_string(), "image".to_string()].as_slice())
        );
        delete_provider("gateway").unwrap();
        assert!(!load_models_config()
            .unwrap()
            .providers
            .contains_key("gateway"));
    }

    #[test]
    fn oauth_token_is_scoped_and_round_trips() {
        let _cfg = TempConfig::new();
        upsert_oauth_token("acme", "access".into(), "refresh".into(), 123).unwrap();
        assert!(matches!(
            read_oauth_token("acme").unwrap(),
            Some(Credential::Oauth { access, refresh, expires })
                if access == "access" && refresh == "refresh" && expires == 123
        ));
        assert!(read_oauth_token("other").unwrap().is_none());
    }

    #[test]
    fn delete_credential_removes_entry() {
        let _cfg = TempConfig::new();
        upsert_credential(
            "anthropic",
            Credential::ApiKey {
                key: Some("k".into()),
                env: None,
            },
        )
        .unwrap();
        assert!(delete_credential("anthropic").unwrap());
        // Second delete is a no-op.
        assert!(!delete_credential("anthropic").unwrap());
        assert!(read_auth().unwrap().is_empty());
    }

    #[test]
    fn load_models_config_missing_is_empty() {
        let _cfg = TempConfig::new();
        let c = load_models_config().unwrap();
        assert!(c.providers.is_empty());
    }

    #[test]
    fn fallback_reader_prefers_primary_and_only_falls_back_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let primary = tmp.path().join("primary.json");
        let fallback = tmp.path().join("fallback.json");
        std::fs::write(&fallback, "fallback").unwrap();

        let (_, text) = read_text_with_fallback(primary.clone(), Some(fallback.clone()))
            .unwrap()
            .expect("fallback should be read");
        assert_eq!(text, "fallback");

        std::fs::write(&primary, "primary").unwrap();
        let (path, text) = read_text_with_fallback(primary.clone(), Some(fallback))
            .unwrap()
            .expect("primary should be read");
        assert_eq!(path, primary);
        assert_eq!(text, "primary");
    }

    #[test]
    fn fallback_reader_does_not_mask_primary_read_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let primary = tmp.path().join("primary.json");
        let fallback = tmp.path().join("fallback.json");
        std::fs::create_dir(&primary).unwrap();
        std::fs::write(&fallback, "fallback").unwrap();

        let error = read_text_with_fallback(primary.clone(), Some(fallback)).unwrap_err();
        assert!(matches!(error, ConfigError::Read { path, .. } if path == primary));
    }

    #[test]
    fn load_models_config_parses_with_comments() {
        let _cfg = TempConfig::new();
        let json = r#"{
  // a one-api style gateway
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "authHeader": true,
      "apiKey": "gw-secret",
      "models": [
        { "id": "claude-sonnet-5", "name": "Sonnet via gateway" }
      ]
    }
  }
}"#;
        std::fs::write(models_path().unwrap(), json).unwrap();
        let c = load_models_config().unwrap();
        let gw = c
            .providers
            .get("gateway")
            .expect("gateway provider present");
        assert_eq!(gw.base_url.as_deref(), Some("https://gw.example.com"));
        assert!(gw.auth_header.unwrap_or(false));
        assert_eq!(gw.models.len(), 1);
        assert_eq!(gw.models[0].id, "claude-sonnet-5");
    }

    #[test]
    fn provider_to_models_merges_headers_without_synth_bearer() {
        // `provider_to_models` merges model-level then provider-level headers,
        // but does NOT synthesize the `authHeader:true` Bearer itself — that
        // happens centrally in `crate::provider::resolve` (via
        // `models_json_bearer_token`) so it can be skipped on the x-api-key
        // path. Here the model carries only what the file declared.
        let cfg = ProviderConfig {
            name: None,
            base_url: Some("https://gw.example.com".into()),
            api_key: Some("gw-secret".into()),
            api: None,
            headers: Some({
                let mut h = BTreeMap::new();
                h.insert("x-portkey-key".into(), "portkey-secret".into());
                h
            }),
            auth_header: Some(true),
            models: vec![ModelDefinition {
                id: "claude-sonnet-5".into(),
                name: None,
                base_url: None,
                reasoning: None,
                thinking_level_map: None,
                context_window: None,
                max_tokens: None,
                cost: None,
                input: None,
                headers: None,
                compat: None,
            }],
        };
        let models = provider_to_models("gateway", &cfg).expect("anthropic-compatible");
        assert_eq!(models.len(), 1);
        let m = &models[0];
        assert_eq!(m.id, "claude-sonnet-5");
        assert_eq!(m.base_url, "https://gw.example.com");
        assert_eq!(m.provider, "gateway");
        let headers = m.headers.as_ref().expect("provider headers merged");
        // Declared provider header folds in…
        assert_eq!(
            headers.get("x-portkey-key").map(|s| s.as_str()),
            Some("portkey-secret")
        );
        // …but no Bearer is synthesized here. The bearer-from-authHeader path
        // is exercised end-to-end by the provider.rs `resolve` tests
        // (`models_json_auth_header_satisfies_auth_without_env`,
        // `api_key_flag_beats_models_json_bearer`).
        assert!(
            headers.get("authorization").is_none(),
            "provider_to_models must not synthesize the Bearer; resolve does"
        );
    }

    #[test]
    fn provider_to_models_supports_openai_completions() {
        let config: ModelsConfig = serde_json::from_str(
            r#"{
                "providers": {
                    "oai": {
                        "api": "openai-completions",
                        "baseUrl": "https://gateway.example.com/v1",
                        "apiKey": "secret",
                        "models": [{"id":"gpt-test","maxTokens":4096}]
                    }
                }
            }"#,
        )
        .unwrap();
        let models = provider_to_models("oai", &config.providers["oai"]).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].api, Api::OpenaiCompletions);
        assert_eq!(models[0].provider, "oai");
        assert_eq!(models[0].max_tokens, 4096);
        assert_eq!(models[0].cost, ModelCost::default());
        assert!(!models[0].has_pricing());
        assert!(
            models[0].headers.is_none(),
            "credentials are bound after provider resolution"
        );
    }

    #[test]
    fn models_json_pricing_reaches_providers_and_survives_serialization() {
        let mut config = parse_models_json(
            r#"{
                "providers": {
                    "pricing-fixture": {
                        "api": "anthropic-messages",
                        "models": [{
                            "id": "priced-model",
                            "cost": {
                                "input": 2, "output": 8, "cacheRead": 0.2, "cacheWrite": 2.5,
                                "tiers": [{
                                    "inputTokensAbove": 1000000,
                                    "input": 4, "output": 16, "cacheRead": 0.4, "cacheWrite": 5
                                }]
                            }
                        }]
                    }
                }
            }"#,
        )
        .unwrap();
        let expected = config.providers["pricing-fixture"].models[0]
            .cost
            .clone()
            .unwrap();
        for api in [
            "anthropic-messages",
            "openai-completions",
            "openai-responses",
        ] {
            let cfg = config.providers.get_mut("pricing-fixture").unwrap();
            cfg.api = Some(api.into());
            let models = provider_to_models("pricing-fixture", cfg).unwrap();
            assert_eq!(models[0].cost, expected);
            assert!(models[0].has_pricing());
            let mut usage = rpi_ai::Usage::zero();
            usage.input = 100_000;
            usage.output = 250_000;
            usage.cache_read = 300_000;
            usage.cache_write = 400_000;
            let cost = rpi_ai::providers::anthropic::cost::calculate_cost(&models[0].cost, &usage);
            assert!((cost.total - 3.26).abs() < 1e-10, "base pricing for {api}");
            usage.input = 1_000_001;
            usage.output = 1_000_000;
            usage.cache_read = 0;
            usage.cache_write = 0;
            let cost = rpi_ai::providers::anthropic::cost::calculate_cost(&models[0].cost, &usage);
            assert!(
                (cost.total - 20.000004).abs() < 1e-10,
                "tiered pricing for {api}"
            );
        }
        let saved = serde_json::to_string(&config).unwrap();
        let reloaded = parse_models_json(&saved).unwrap();
        assert_eq!(
            reloaded.providers["pricing-fixture"].models[0].cost,
            Some(expected)
        );
        let cfg = config.providers.get_mut("pricing-fixture").unwrap();
        cfg.models[0].cost = Some(ModelCost::default());
        let free = provider_to_models("pricing-fixture", cfg).unwrap();
        assert_eq!(free[0].cost, ModelCost::default());
        assert!(
            free[0].has_pricing(),
            "explicit zero prices are still configured"
        );
    }

    #[test]
    fn models_json_compat_reaches_the_model_for_openai_completions() {
        // The `compat` block is what lets a custom openai-completions provider
        // describe its quirks. `requiresThinkingAsText` is the one that decides
        // whether the model's own reasoning is sent back or dropped — see
        // `docs/debugging/llm-repetition-forensics.md` §八. If this hop ever breaks, the
        // setting silently does nothing (the provider falls back to
        // `Content::text_only`, which drops every thinking block).
        let config: ModelsConfig = serde_json::from_str(
            r#"{
                "providers": {
                    "qwen": {
                        "api": "openai-completions",
                        "baseUrl": "https://coding.example.com/v1",
                        "apiKey": "secret",
                        "models": [{
                            "id": "qwen3.7-plus",
                            "maxTokens": 131072,
                            "thinkingLevelMap": { "xhigh": "xhigh", "max": "max" },
                            "compat": { "requiresThinkingAsText": true }
                        }]
                    }
                }
            }"#,
        )
        .unwrap();
        let models = provider_to_models("qwen", &config.providers["qwen"]).unwrap();
        assert_eq!(
            models[0]
                .thinking_level_map
                .as_ref()
                .unwrap()
                .get(&rpi_ai::ThinkingLevel::Xhigh),
            Some(&Some("xhigh".into()))
        );
        match models[0].compat.as_ref().expect("compat must survive") {
            StreamingProtocolCompat::OpenaiCompletions(compat) => {
                assert_eq!(compat.requires_thinking_as_text, Some(true));
            }
            other => panic!("expected OpenaiCompletions compat, got {other:?}"),
        }

        // An entry without `compat` stays on the default (thinking dropped).
        let plain: ModelsConfig = serde_json::from_str(
            r#"{
                "providers": {
                    "qwen": {
                        "api": "openai-completions",
                        "baseUrl": "https://coding.example.com/v1",
                        "apiKey": "secret",
                        "models": [{"id":"qwen3.7-plus"}]
                    }
                }
            }"#,
        )
        .unwrap();
        let plain_models = provider_to_models("qwen", &plain.providers["qwen"]).unwrap();
        assert!(plain_models[0].compat.is_none());
    }

    #[test]
    fn models_json_accepts_pi_deepseek_compat_and_warns_on_unknown_or_invalid_fields() {
        let mut config: ModelsConfig = serde_json::from_value(serde_json::json!({
            "providers": { "deepseek": {
                "api": "openai-completions",
                "models": [{ "id": "reasoner", "reasoning": true, "compat": {
                    "thinkingFormat": "deepseek",
                    "requiresReasoningContentOnAssistantMessages": true
                }}]
            }}
        }))
        .unwrap();
        assert!(model_compat_warnings(&config).is_empty());
        let models = provider_to_models("deepseek", &config.providers["deepseek"]).unwrap();
        assert!(rpi_ai::model::prior_reasoning_is_replayed(&models[0]));
        let Some(StreamingProtocolCompat::OpenaiCompletions(compat)) = &models[0].compat else {
            panic!("missing model compatibility flags");
        };
        assert_eq!(compat.requires_reasoning_content, Some(true));
        assert_eq!(
            compat.thinking_format,
            Some(rpi_ai::model::OpenaiThinkingFormat::Deepseek)
        );

        config.providers["deepseek"].models[0]
            .compat
            .as_mut()
            .unwrap()["misspelledField"] = serde_json::json!("secret-fixture-value");
        let warnings = model_compat_warnings(&config);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("deepseek") && warnings[0].contains("reasoner"));
        assert!(warnings[0].contains("unknown field \"misspelledField\""));
        assert!(!warnings[0].contains("secret-fixture-value"));

        config.providers["deepseek"].models[0].compat =
            Some(serde_json::json!({"requiresReasoningContent": "secret-fixture-token"}));
        let warning = &model_compat_warnings(&config)[0];
        assert!(warning.contains("invalid configuration"));
        assert!(!warning.contains("secret-fixture-token"));
        config.providers["deepseek"].models[0].compat =
            Some(serde_json::json!({"thinkingFormat": "unsupported"}));
        assert!(model_compat_warnings(&config)[0].contains("invalid configuration"));
    }

    #[test]
    fn responses_compat_diagnostics_use_the_responses_schema() {
        let config: ModelsConfig = serde_json::from_value(serde_json::json!({
            "providers": { "relay": {
                "api": "openai-responses", "models": [{ "id": "test", "compat": {
                    "supportsOpenAIGrammarTools": true, "supportsStore": false
                }}]
            }}
        }))
        .unwrap();
        let warnings = model_compat_warnings(&config);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("supportsStore"));
    }

    #[test]
    fn native_reasoning_content_key_wins_over_a_conflicting_pi_alias() {
        let config: ModelsConfig = serde_json::from_value(serde_json::json!({
            "providers": { "relay": {
                "api": "openai-completions", "models": [{ "id": "test", "compat": {
                    "requiresReasoningContent": false,
                    "requiresReasoningContentOnAssistantMessages": true
                }}]
            }}
        }))
        .unwrap();
        let warnings = model_compat_warnings(&config);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("takes precedence"));
        let models = provider_to_models("relay", &config.providers["relay"]).unwrap();
        let Some(StreamingProtocolCompat::OpenaiCompletions(compat)) = &models[0].compat else {
            panic!("valid native flag must survive an alias conflict");
        };
        assert_eq!(compat.requires_reasoning_content, Some(false));
    }

    #[test]
    fn custom_openai_provider_resolves_explicit_api_key_environment_reference() {
        let _guard = env_lock().lock().unwrap();
        let env_name = "RPI_FAKE_PROVIDER_API_KEY";
        std::env::set_var(env_name, "env-secret");
        let cfg = ProviderConfig {
            name: None,
            base_url: Some("https://gateway.example.com/v1".into()),
            api_key: Some("$RPI_FAKE_PROVIDER_API_KEY".into()),
            api: Some("openai-completions".into()),
            headers: None,
            auth_header: None,
            models: vec![ModelDefinition {
                id: "fake-model".into(),
                name: None,
                base_url: None,
                reasoning: None,
                thinking_level_map: None,
                context_window: None,
                max_tokens: None,
                cost: None,
                input: None,
                headers: None,
                compat: None,
            }],
        };
        let models = provider_to_models("rpi-fake-provider", &cfg).unwrap();
        assert_eq!(
            openai_provider_api_key("rpi-fake-provider", &cfg).as_deref(),
            Some("env-secret")
        );
        assert!(
            models[0].headers.is_none(),
            "discovery does not bind credentials"
        );
        std::env::remove_var(env_name);
    }

    #[test]
    fn openai_protocol_ids_do_not_inherit_the_official_env_key() {
        let _guard = env_lock().lock().unwrap();
        let prev_openai = std::env::var_os("OPENAI_API_KEY");
        let prev_completions = std::env::var_os("OPENAI_COMPLETIONS_API_KEY");
        let prev_responses = std::env::var_os("OPENAI_RESPONSES_API_KEY");
        std::env::set_var("OPENAI_API_KEY", "official-key");
        std::env::remove_var("OPENAI_COMPLETIONS_API_KEY");
        std::env::remove_var("OPENAI_RESPONSES_API_KEY");
        let config = ProviderConfig {
            name: None,
            base_url: Some("https://gateway.example.com/v1".into()),
            api_key: None,
            api: Some("openai-responses".into()),
            headers: None,
            auth_header: None,
            models: Vec::new(),
        };

        assert_eq!(
            openai_provider_api_key("openai", &config).as_deref(),
            Some("official-key")
        );
        assert!(openai_provider_api_key("openai-completions", &config).is_none());
        assert!(openai_provider_api_key("openai-responses", &config).is_none());
        assert!(openai_provider_api_key("OpenAI", &config).is_none());

        restore_env("OPENAI_API_KEY", prev_openai);
        restore_env("OPENAI_COMPLETIONS_API_KEY", prev_completions);
        restore_env("OPENAI_RESPONSES_API_KEY", prev_responses);
    }

    #[test]
    fn native_provider_api_key_env_mapping_uses_exact_provider_ids() {
        assert_eq!(
            native_provider_api_key_env_var("openai"),
            Some("OPENAI_API_KEY")
        );
        for provider in [
            "deepseek",
            "openrouter",
            "google",
            "groq",
            "qwen",
            "nvidia",
            "OpenAI",
            "openai-completions",
        ] {
            assert_eq!(
                native_provider_api_key_env_var(provider),
                None,
                "external providers must declare auth in their package or models.json"
            );
        }
    }

    #[test]
    fn malformed_auth_json_is_an_error_not_silent_empty() {
        let _cfg = TempConfig::new();
        std::fs::write(auth_path().unwrap(), "{ not json").unwrap();
        assert!(matches!(read_auth(), Err(ConfigError::Json { .. })));
    }

    #[test]
    fn agent_dir_nests_under_agent_by_default() {
        // With no env override, agent_dir() must end in `.../.rpi/agent`
        // (mirrors pi's `getAgentDir`). We can't assertion the home prefix
        // portably, but the leaf two segments are stable.
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var_os(CONFIG_DIR_ENV);
        std::env::remove_var(CONFIG_DIR_ENV);
        let dir = agent_dir().unwrap();
        restore_env(CONFIG_DIR_ENV, prev);
        assert!(dir.ends_with("agent"));
        assert!(dir
            .parent()
            .map(|p| p.ends_with(CONFIG_DIR_NAME))
            .unwrap_or(false));
    }

    #[test]
    fn migrate_legacy_layout_moves_flat_files_into_agent() {
        // Drive the core migration directly against a temp root/agent so the
        // result is independent of whatever RPI_CODING_AGENT_DIR the parallel
        // TempConfig tests happen to set.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let agent = root.join("agent");
        std::fs::write(root.join("auth.json"), "{}").unwrap();
        std::fs::write(root.join("models.json"), "{}").unwrap();
        std::fs::write(root.join(".setup_done"), "1").unwrap();
        let moved = migrate_legacy_layout_in(&root, &agent).unwrap();
        assert_eq!(moved, 3);
        assert!(agent.join("auth.json").exists());
        assert!(agent.join("models.json").exists());
        assert!(agent.join(".setup_done").exists());
        assert!(!root.join("auth.json").exists());
    }

    #[test]
    fn migrate_legacy_layout_noop_when_agent_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let agent = root.join("agent");
        std::fs::write(root.join("auth.json"), "{}").unwrap();
        std::fs::create_dir_all(&agent).unwrap();
        let moved = migrate_legacy_layout_in(&root, &agent).unwrap();
        assert_eq!(moved, 0); // agent/ already present — leave flat file alone
    }

    #[test]
    fn migrate_legacy_layout_noop_when_no_flat_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let agent = root.join("agent");
        let moved = migrate_legacy_layout_in(&root, &agent).unwrap();
        assert_eq!(moved, 0);
    }

    #[test]
    fn migrate_legacy_layout_public_skips_env_override() {
        // When RPI_CODING_AGENT_DIR is set, the public entry point is a no-op
        // (it must never touch an explicit override). TempConfig sets it.
        let _cfg = TempConfig::new();
        let moved = migrate_legacy_layout().unwrap();
        assert_eq!(moved, 0);
    }

    #[test]
    fn read_trust_missing_file_is_empty() {
        let _cfg = TempConfig::new();
        assert!(read_trust().unwrap().is_empty());
    }

    #[test]
    fn read_trust_parses_decisions() {
        let _cfg = TempConfig::new();
        let path = trust_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{ "/home/me/proj": true, "/home/me/untrusted": false, "/home/me/null": null }"#,
        )
        .unwrap();
        let store = read_trust().unwrap();
        assert_eq!(store.len(), 3);
        assert_eq!(store.get("/home/me/proj").copied().flatten(), Some(true));
        assert_eq!(
            store.get("/home/me/untrusted").copied().flatten(),
            Some(false)
        );
        assert_eq!(store.get("/home/me/null").copied().flatten(), None);
    }

    #[test]
    fn resolve_config_value_literal_passthrough() {
        assert_eq!(
            resolve_config_value("sk-literal-key", None),
            Some("sk-literal-key".into())
        );
    }

    #[test]
    fn resolve_config_value_env_var() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var_os("RPI_TEST_CFG_KEY");
        let prev2 = std::env::var_os("RPI_TEST_CFG_KEY2");
        std::env::set_var("RPI_TEST_CFG_KEY", "secret-from-env");
        assert_eq!(
            resolve_config_value("$RPI_TEST_CFG_KEY", None),
            Some("secret-from-env".into())
        );
        assert_eq!(
            resolve_config_value("prefix-${RPI_TEST_CFG_KEY}-suffix", None),
            Some("prefix-secret-from-env-suffix".into())
        );
        // Two vars in one template.
        std::env::set_var("RPI_TEST_CFG_KEY2", "two");
        assert_eq!(
            resolve_config_value("a-$RPI_TEST_CFG_KEY-b-$RPI_TEST_CFG_KEY2-c", None),
            Some("a-secret-from-env-b-two-c".into())
        );
        // Env overlay wins over process env.
        let mut overlay = BTreeMap::new();
        overlay.insert("RPI_TEST_CFG_KEY".into(), "overlay-value".into());
        assert_eq!(
            resolve_config_value("$RPI_TEST_CFG_KEY", Some(&overlay)),
            Some("overlay-value".into())
        );
        restore_env("RPI_TEST_CFG_KEY", prev);
        restore_env("RPI_TEST_CFG_KEY2", prev2);
    }

    #[test]
    fn resolve_config_value_unset_env_is_none() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var_os("RPI_TEST_CFG_ABSENT");
        std::env::remove_var("RPI_TEST_CFG_ABSENT");
        // Any referenced unset var ⇒ the whole value is None (pi semantics).
        assert_eq!(resolve_config_value("$RPI_TEST_CFG_ABSENT", None), None);
        assert_eq!(
            resolve_config_value("prefix-$RPI_TEST_CFG_ABSENT-suffix", None),
            None
        );
        restore_env("RPI_TEST_CFG_ABSENT", prev);
    }

    #[test]
    fn resolve_config_value_dollar_dollar_escapes_literal() {
        assert_eq!(
            resolve_config_value("price-$$5", None),
            Some("price-$5".into())
        );
        assert_eq!(resolve_config_value("$!bang", None), Some("!bang".into()));
    }

    #[test]
    fn resolve_config_value_command_runs_shell() {
        // `!echo resolved` → "resolved" (sh on Unix; `echo` works under cmd too).
        assert_eq!(
            resolve_config_value_uncached("!echo rpi-cfg-resolved", None),
            Some("rpi-cfg-resolved".into())
        );
        // Non-zero exit ⇒ None.
        assert_eq!(resolve_config_value_uncached("!false", None), None);
    }

    #[test]
    fn resolve_headers_drops_unresolvable() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var_os("RPI_TEST_HDR_SET");
        std::env::set_var("RPI_TEST_HDR_SET", "set-value");
        let mut h = BTreeMap::new();
        h.insert("x-set".into(), "$RPI_TEST_HDR_SET".into());
        h.insert("x-unset".into(), "$RPI_TEST_HDR_UNSET".into());
        h.insert("x-literal".into(), "literal-value".into());
        let resolved = resolve_headers(&h, None);
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved.get("x-set").map(|s| s.as_str()), Some("set-value"));
        assert_eq!(
            resolved.get("x-literal").map(|s| s.as_str()),
            Some("literal-value")
        );
        assert!(!resolved.contains_key("x-unset"));
        restore_env("RPI_TEST_HDR_SET", prev);
    }

    #[test]
    fn agent_dir_respects_env_override() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var_os(CONFIG_DIR_ENV);
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var(CONFIG_DIR_ENV, tmp.path());
        let dir = agent_dir().unwrap();
        restore_env(CONFIG_DIR_ENV, prev);
        assert_eq!(dir, tmp.path());
    }

    #[test]
    fn relative_override_is_rejected() {
        let _guard = env_lock().lock().unwrap();
        let prev = std::env::var_os(CONFIG_DIR_ENV);
        std::env::set_var(CONFIG_DIR_ENV, "relative/path");
        let err = agent_dir().unwrap_err();
        restore_env(CONFIG_DIR_ENV, prev);
        assert!(matches!(err, ConfigError::RelativeOverride { .. }));
    }

    /// Restore/remove an env var based on its prior `OsString` value.
    fn restore_env(name: &str, prev: Option<std::ffi::OsString>) {
        match prev {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
    }
}

/// Convert host configuration with the same rules as the embeddable runtime.
pub fn provider_to_models(provider_id: &str, cfg: &ProviderConfig) -> Option<Vec<Model>> {
    rpi_runtime::config::provider_to_models(provider_id, cfg, &crate::provider::CliValues)
}
