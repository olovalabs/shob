//! Shared, provider-scoped credential resolution for requests and auth UI.
//! Provider packages own their environment names and anonymous policy. The host
//! owns persistence and precedence, and never guesses a key from a wire protocol.
use crate::config::{AuthStore, Credential, ProviderConfig};
use crate::ProviderDeclaration as ProviderManifest;
use crate::ValueSource;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum CredentialSource {
    /// A host-supplied override scoped to the selected provider.
    Explicit,
    StoredApiKey,
    StoredOauth,
    ModelsJson,
    Environment(String),
    Headers,
    Anonymous,
    Missing,
}

/// Local readiness only; this does not claim that a server accepted the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AuthStatus {
    Missing,
    Ready,
    Expired,
    Anonymous,
}

/// Safe for UI snapshots and change notifications; contains no credential values.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct AuthState {
    pub status: AuthStatus,
    pub source: CredentialSource,
}

impl AuthState {
    pub fn is_ready(&self) -> bool {
        matches!(self.status, AuthStatus::Ready | AuthStatus::Anonymous)
    }
}

pub(crate) fn state(
    provider: &str,
    config: Option<&ProviderConfig>,
    store: &AuthStore,
    manifest: Option<&ProviderManifest>,
    values: &dyn ValueSource,
    refreshable: bool,
    has_headers: impl FnOnce() -> bool,
) -> AuthState {
    let credential = resolve_refreshable(provider, config, store, manifest, values, refreshable);
    let source = if credential.source.is_present() {
        credential.source
    } else if has_headers() {
        CredentialSource::Headers
    } else {
        CredentialSource::Missing
    };
    let status = match source {
        CredentialSource::Anonymous => AuthStatus::Anonymous,
        CredentialSource::Missing => match store.get(provider) {
            Some(Credential::Oauth { expires, .. })
                if *expires != 0 && *expires <= values.now_unix_seconds() =>
            {
                AuthStatus::Expired
            }
            _ => AuthStatus::Missing,
        },
        _ => AuthStatus::Ready,
    };
    AuthState { status, source }
}

impl CredentialSource {
    pub fn is_present(&self) -> bool {
        !matches!(self, Self::Missing)
    }

    pub fn as_json_str(&self) -> &str {
        match self {
            Self::Explicit => "override",
            Self::StoredApiKey => "auth.json",
            Self::StoredOauth => "oauth",
            Self::ModelsJson => "models.json",
            Self::Environment(name) => name,
            Self::Headers => "headers",
            Self::Anonymous => "anonymous",
            Self::Missing => "none",
        }
    }
}

/// Seconds before expiry at which a stored OAuth credential is renewed, so a
/// token cannot expire between the check and the provider's request.
pub const OAUTH_REFRESH_MARGIN_SECS: i64 = 60;

/// Whether a stored OAuth credential expiring at `expires` (Unix seconds, `0`
/// for never) should be renewed before it is used at `now`.
pub fn oauth_needs_refresh(expires: i64, now: i64) -> bool {
    expires != 0 && expires <= now.saturating_add(OAUTH_REFRESH_MARGIN_SECS)
}

/// A renewed OAuth access token, already persisted by the host.
// Deliberately no Debug: this object contains secret material.
pub struct RefreshedOAuth {
    pub access: String,
    /// Unix seconds; `0` means the credential does not expire.
    pub expires: i64,
}

/// Host-owned renewal of stored OAuth credentials, typically through the
/// provider package that issued them. Requests call it before sending a stored
/// OAuth credential that has expired or is about to.
pub trait OAuthRefresher: Send + Sync {
    /// Whether the host can renew `provider`'s stored OAuth credential.
    fn supports(&self, provider: &str) -> bool;
    /// Renew and persist `provider`'s stored credential. This may block, so the
    /// runtime calls it off the async workers. The error is shown to the user
    /// and must not contain credential material.
    fn refresh(&self, provider: &str) -> Result<RefreshedOAuth, String>;
}

// Deliberately no Debug: this object contains secret material.
pub struct ResolvedCredential {
    pub key: Option<String>,
    pub source: CredentialSource,
    pub bearer: bool,
}

/// Only these identities have a built-in environment fallback. Other providers
/// need a loaded declaration or an explicit models.json apiKey reference.
pub fn builtin_api_key_env(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        "google" => Some("GEMINI_API_KEY"),
        _ => None,
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|key| !key.trim().is_empty())
}

pub fn resolve(
    provider: &str,
    config: Option<&ProviderConfig>,
    store: &AuthStore,
    manifest: Option<&ProviderManifest>,
    values: &dyn ValueSource,
) -> ResolvedCredential {
    resolve_refreshable(provider, config, store, manifest, values, false)
}

/// As [`resolve`], except that an expired stored OAuth credential still counts
/// when `refreshable`: the execution binding renews it before it is sent.
pub(crate) fn resolve_refreshable(
    provider: &str,
    config: Option<&ProviderConfig>,
    store: &AuthStore,
    manifest: Option<&ProviderManifest>,
    values: &dyn ValueSource,
    refreshable: bool,
) -> ResolvedCredential {
    let bearer = config
        .and_then(|config| config.auth_header)
        .unwrap_or(false);
    let credential = |key, source, bearer| ResolvedCredential {
        key: Some(key),
        source,
        bearer,
    };
    match store.get(provider) {
        Some(Credential::ApiKey {
            key: Some(key),
            env,
        }) => {
            if let Some(key) = nonempty(crate::values::resolve_config_value(
                key,
                env.as_ref(),
                values,
                true,
            )) {
                return credential(key, CredentialSource::StoredApiKey, bearer);
            }
        }
        Some(Credential::Oauth {
            access,
            refresh,
            expires,
        }) if !access.trim().is_empty() => {
            let now = values.now_unix_seconds();
            if *expires == 0 || *expires > now || (refreshable && !refresh.trim().is_empty()) {
                return credential(access.clone(), CredentialSource::StoredOauth, true);
            }
        }
        _ => {}
    }
    if let Some(key) = config
        .and_then(|config| config.api_key.as_deref())
        .and_then(|raw| nonempty(crate::values::resolve_config_value(raw, None, values, true)))
    {
        return credential(key, CredentialSource::ModelsJson, bearer);
    }
    let builtin = crate::catalog::builtin_provider(provider);
    // Discovery declarations repeat the built-in primary env name. Retain that
    // provider's aliases; a different extension-owned policy remains authoritative.
    let native_policy = manifest.is_none_or(|manifest| {
        builtin.is_some_and(|builtin| {
            manifest.api_key_env.as_deref() == builtin.env_vars.first().map(String::as_str)
        })
    });
    if native_policy && provider == "anthropic" {
        if let Some(key) = nonempty(values.env("ANTHROPIC_AUTH_TOKEN")) {
            return credential(
                key,
                CredentialSource::Environment("ANTHROPIC_AUTH_TOKEN".into()),
                true,
            );
        }
    }
    if let Some(manifest) = manifest {
        if let Some(name) = manifest.api_key_env.as_deref() {
            if let Some(key) = nonempty(values.env(name)) {
                return credential(key, CredentialSource::Environment(name.into()), bearer);
            }
        }
    }
    if native_policy {
        if let Some(builtin) = builtin {
            for name in &builtin.env_vars {
                if manifest.and_then(|manifest| manifest.api_key_env.as_ref()) == Some(name) {
                    continue;
                }
                if let Some(key) = nonempty(values.env(name)) {
                    return credential(key, CredentialSource::Environment(name.clone()), bearer);
                }
            }
        } else if let Some(name) = builtin_api_key_env(provider) {
            if let Some(key) = nonempty(values.env(name)) {
                return credential(key, CredentialSource::Environment(name.into()), bearer);
            }
        }
    }
    if let Some(source) = crate::cloud_auth::configured(provider, values) {
        return ResolvedCredential {
            key: None,
            source: CredentialSource::Environment(source.into()),
            bearer: false,
        };
    }
    ResolvedCredential {
        key: None,
        source: if manifest
            .map(|manifest| !manifest.requires_auth)
            .unwrap_or_else(|| {
                crate::catalog::builtin_provider(provider)
                    .is_some_and(|provider| !provider.requires_auth)
            }) {
            CredentialSource::Anonymous
        } else {
            CredentialSource::Missing
        },
        bearer,
    }
}

pub fn has_header_auth(headers: Option<&std::collections::BTreeMap<String, String>>) -> bool {
    headers.is_some_and(|headers| {
        headers.iter().any(|(name, value)| {
            ["authorization", "x-api-key", "cf-aig-authorization"]
                .contains(&name.to_ascii_lowercase().as_str())
                && !value.trim().is_empty()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn discovered_native_declaration_keeps_environment_aliases() {
        let builtin = crate::catalog::builtin_provider("charm-hyper").unwrap();
        let manifest = crate::ProviderDeclaration {
            id: builtin.id.clone(),
            models: Vec::new(),
            api_key_env: builtin.env_vars.first().cloned(),
            requires_auth: true,
        };
        let values = BTreeMap::from([("HYPER_API_KEY".into(), "test-key".into())]);
        let credential = resolve(
            "charm-hyper",
            None,
            &AuthStore::new(),
            Some(&manifest),
            &values,
        );
        assert_eq!(credential.key.as_deref(), Some("test-key"));
        assert_eq!(
            credential.source,
            CredentialSource::Environment("HYPER_API_KEY".into())
        );
    }

    #[test]
    fn explicit_extension_environment_policy_remains_authoritative() {
        let manifest = crate::ProviderDeclaration {
            id: "charm-hyper".into(),
            models: Vec::new(),
            api_key_env: Some("PRIVATE_HYPER_KEY".into()),
            requires_auth: true,
        };
        let values = BTreeMap::from([("HYPER_API_KEY".into(), "not-selected".into())]);
        let credential = resolve(
            "charm-hyper",
            None,
            &AuthStore::new(),
            Some(&manifest),
            &values,
        );
        assert!(credential.key.is_none());
        assert_eq!(credential.source, CredentialSource::Missing);
    }
}
