pub mod api_key;
pub mod callback;
pub mod device_code;
pub mod oauth;
pub mod pkce;
pub mod storage;

use crate::catalog::get_provider;
use crate::data::ALL_PROVIDERS;
use crate::types::{AuthStatus, AuthType};

/// Determine the live authentication readiness and credential source for a provider, using the active credentials in the private runtime.
pub fn check_auth_with_active(
    provider_id: &str,
    active: &std::collections::HashSet<String>,
) -> AuthStatus {
    let spec = match get_provider(provider_id) {
        Some(spec) => spec,
        None => {
            if let Some(cred) = storage::get_local_credential(provider_id) {
                let source = match cred {
                    crate::types::StoredCredential::OAuth { .. } => {
                        "Stored OAuth token".to_string()
                    }
                    crate::types::StoredCredential::ApiKey { source, .. } => {
                        source.unwrap_or_else(|| "Stored API key".to_string())
                    }
                };
                return AuthStatus {
                    provider: provider_id.to_string(),
                    ready: true,
                    source: Some(source),
                    auth_type: AuthType::ApiKey,
                };
            }
            return AuthStatus {
                provider: provider_id.to_string(),
                ready: false,
                source: None,
                auth_type: AuthType::None,
            };
        }
    };

    let auth_type = spec.auth_type();

    // 1. Check if OAuth token or credential is in local storage (instant in-memory/file check)
    if let Some(cred) = storage::get_local_credential(&spec.id) {
        let source = match cred {
            crate::types::StoredCredential::OAuth { .. } => "Stored OAuth token".to_string(),
            crate::types::StoredCredential::ApiKey { ref source, .. } => source
                .clone()
                .unwrap_or_else(|| "Stored API key".to_string()),
        };
        return AuthStatus {
            provider: spec.id.clone(),
            ready: true,
            source: Some(source),
            auth_type,
        };
    }

    // 2. Check the private runtime credential store
    if active.contains(&spec.id) {
        return AuthStatus {
            provider: spec.id.clone(),
            ready: true,
            source: Some("Kathamo credential".to_string()),
            auth_type,
        };
    }

    // 3. Check environment variables for API key
    if let Some(res) = api_key::resolve_api_key(spec) {
        return AuthStatus {
            provider: spec.id.clone(),
            ready: true,
            source: Some(res.source),
            auth_type,
        };
    }

    // Provider is not authenticated
    AuthStatus {
        provider: spec.id.clone(),
        ready: false,
        source: None,
        auth_type,
    }
}

/// Determine the live authentication readiness and credential source for a provider.
pub fn check_auth(provider_id: &str) -> AuthStatus {
    let active = storage::authenticated_providers();
    check_auth_with_active(provider_id, &active)
}

/// Check authentication readiness for all 103 providers in batch.
pub fn check_all_auth() -> Vec<AuthStatus> {
    let active = storage::authenticated_providers();
    ALL_PROVIDERS
        .iter()
        .map(|spec| check_auth_with_active(&spec.id, &active))
        .collect()
}
