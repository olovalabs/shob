//! CLI adapter for the shared provider-scoped credential resolver.
#[cfg(test)]
use crate::config::{self, Credential};
use crate::config::{AuthStore, ProviderConfig};
use rpi_extensions::ProviderManifest;
#[cfg(test)]
pub(crate) use rpi_runtime::credentials::has_header_auth;
pub(crate) use rpi_runtime::credentials::{
    builtin_api_key_env, CredentialSource, ResolvedCredential,
};
pub(crate) fn resolve(
    provider: &str,
    config: Option<&ProviderConfig>,
    store: &AuthStore,
    manifest: Option<&ProviderManifest>,
) -> ResolvedCredential {
    let declaration = manifest.map(crate::provider::declaration);
    rpi_runtime::credentials::resolve(
        provider,
        config,
        store,
        declaration.as_ref(),
        &crate::provider::CliValues,
    )
}

pub(crate) fn status(
    provider: &str,
    models: &crate::config::ModelsConfig,
    store: &AuthStore,
    manifests: &[ProviderManifest],
) -> rpi_runtime::credentials::AuthState {
    let mut providers = rpi_runtime::ProviderRegistry::default();
    for manifest in manifests {
        providers.declare(crate::provider::declaration(manifest));
    }
    rpi_runtime::RuntimeConfig {
        models: models.clone(),
        credentials: store.clone(),
        ..Default::default()
    }
    .auth_status(provider, &providers, &crate::provider::CliValues)
}

pub(crate) fn statuses(
    models: &crate::config::ModelsConfig,
    store: &AuthStore,
    manifests: &[ProviderManifest],
) -> std::collections::BTreeMap<String, rpi_runtime::credentials::AuthState> {
    let mut providers = rpi_runtime::ProviderRegistry::default();
    for manifest in manifests {
        providers.declare(crate::provider::declaration(manifest));
    }
    rpi_runtime::RuntimeConfig {
        models: models.clone(),
        credentials: store.clone(),
        ..Default::default()
    }
    .auth_states(&providers, &crate::provider::CliValues)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_environment_requires_an_explicit_package_declaration() {
        let _guard = config::test_support::env_lock().lock().unwrap();
        let previous = std::env::var_os("DEEPSEEK_API_KEY");
        std::env::set_var("DEEPSEEK_API_KEY", "fixture-package-key");
        let store = AuthStore::new();
        let without_package = resolve("deepseek", None, &store, None);
        let manifest = ProviderManifest {
            id: "deepseek".into(),
            models: vec![],
            api_key_env: Some("DEEPSEEK_API_KEY".into()),
            requires_auth: true,
        };
        let with_package = resolve("deepseek", None, &store, Some(&manifest));
        match previous {
            Some(value) => std::env::set_var("DEEPSEEK_API_KEY", value),
            None => std::env::remove_var("DEEPSEEK_API_KEY"),
        }
        assert_eq!(without_package.source, CredentialSource::Missing);
        assert!(without_package.key.is_none());
        assert_eq!(
            with_package.source,
            CredentialSource::Environment("DEEPSEEK_API_KEY".into())
        );
        assert_eq!(with_package.key.as_deref(), Some("fixture-package-key"));
    }

    #[test]
    fn package_policy_can_disable_a_builtin_environment_fallback() {
        let _guard = config::test_support::env_lock().lock().unwrap();
        let previous = std::env::var_os("OPENAI_API_KEY");
        std::env::set_var("OPENAI_API_KEY", "unrelated-builtin-key");
        let manifest = ProviderManifest {
            id: "openai".into(),
            models: vec![],
            api_key_env: None,
            requires_auth: true,
        };
        let resolved = resolve("openai", None, &AuthStore::new(), Some(&manifest));
        match previous {
            Some(value) => std::env::set_var("OPENAI_API_KEY", value),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
        assert_eq!(resolved.source, CredentialSource::Missing);
        assert!(resolved.key.is_none());
    }

    #[test]
    fn empty_keys_do_not_block_fallback_or_make_required_providers_ready() {
        let config: ProviderConfig =
            serde_json::from_value(serde_json::json!({"apiKey":"   ","models":[]})).unwrap();
        let mut store = AuthStore::new();
        store.insert(
            "gateway".into(),
            Credential::ApiKey {
                key: Some("".into()),
                env: None,
            },
        );
        let mut manifest = ProviderManifest {
            id: "gateway".into(),
            models: vec![],
            api_key_env: None,
            requires_auth: true,
        };
        assert_eq!(
            resolve("gateway", Some(&config), &store, Some(&manifest)).source,
            CredentialSource::Missing
        );
        manifest.requires_auth = false;
        assert_eq!(
            resolve("gateway", Some(&config), &store, Some(&manifest)).source,
            CredentialSource::Anonymous
        );
    }
}
