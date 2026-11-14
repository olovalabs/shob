//! Host-independent model configuration, authentication and execution bindings.
//!
//! Hosts load their own configuration and credentials, supply a [`ValueSource`],
//! and adapt any already-loaded extension providers through [`ProviderRegistry`].
//! The library does not discover files, load plugins, or depend on a terminal.
pub mod catalog;
pub(crate) mod cloud_auth;
pub mod config;
pub mod credentials;
pub mod provider;
mod runtime;
pub mod thinking;
pub mod values;

pub use provider::{ResolveError, ResolvedModel, RuntimeFactories as ProviderRegistry};
pub use runtime::{ModelIdentity, ModelSelection, Runtime, RuntimeSnapshot};
pub use values::ValueSource;

/// Provider metadata supplied by the host's existing extension registry.
#[derive(Clone)]
pub struct ProviderDeclaration {
    pub id: String,
    pub models: Vec<rpi_ai::Model>,
    pub api_key_env: Option<String>,
    pub requires_auth: bool,
}

/// Model defaults supplied by the host, independent of its storage format.
#[derive(Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSettings {
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub default_thinking_level: Option<String>,
    /// Opaque presentation hint, carried for existing CLI API compatibility.
    pub theme: Option<String>,
    pub http_idle_timeout_ms: Option<i64>,
}

/// An explicit selection, separate from defaults persisted by the host.
#[derive(Default)]
pub struct ModelRequest<'a> {
    pub provider: Option<&'a str>,
    /// Select a wire protocol when a provider exposes the same ID through several APIs.
    pub api: Option<rpi_ai::Api>,
    pub model: Option<&'a str>,
    pub thinking: Option<rpi_ai::ThinkingLevel>,
    pub api_key: Option<&'a str>,
    pub base_url: Option<&'a str>,
}

/// Immutable configuration inputs read from any host-owned storage.
/// Keep this object private: it contains credentials and unexpanded headers.
#[derive(Clone, Default)]
pub struct RuntimeConfig {
    pub models: config::ModelsConfig,
    pub credentials: config::AuthStore,
    pub settings: RuntimeSettings,
}

impl RuntimeConfig {
    /// Resolve each provider independently, expanding its headers only if needed.
    pub fn auth_states(
        &self,
        providers: &ProviderRegistry,
        values: &dyn ValueSource,
    ) -> std::collections::BTreeMap<String, credentials::AuthState> {
        let mut ids: std::collections::BTreeSet<String> = provider::builtin_catalog()
            .into_iter()
            .map(|model| model.provider)
            .collect();
        ids.extend(
            catalog::builtin_providers()
                .iter()
                .map(|provider| provider.id.clone()),
        );
        ids.extend(self.models.providers.keys().cloned());
        ids.extend(self.credentials.keys().cloned());
        ids.extend(
            providers
                .manifests
                .iter()
                .map(|manifest| manifest.id.clone()),
        );
        ids.into_iter()
            .map(|id| {
                let state = self.auth_status(&id, providers, values);
                (id, state)
            })
            .collect()
    }
    /// Local, provider-scoped readiness using the same rules as execution.
    pub fn auth_status(
        &self,
        provider: &str,
        providers: &ProviderRegistry,
        values: &dyn ValueSource,
    ) -> credentials::AuthState {
        credentials::state(
            provider,
            self.models.providers.get(provider),
            &self.credentials,
            providers
                .manifests
                .iter()
                .find(|manifest| manifest.id == provider),
            values,
            providers.refreshes_oauth(provider),
            || {
                provider::provider_has_auth_headers(
                    provider,
                    &self.models,
                    &providers.manifests,
                    values,
                )
            },
        )
    }
    /// Select and bind a runnable model. Existing results retain their providers
    /// and loader leases even when the host later replaces this configuration.
    pub fn resolve(
        &self,
        request: &ModelRequest<'_>,
        providers: &ProviderRegistry,
        values: &dyn ValueSource,
    ) -> Result<ResolvedModel, ResolveError> {
        provider::resolve_with_factories(request, self, providers, values)
    }

    /// All configured models, including those awaiting credentials. Descriptors
    /// and diagnostics omit request headers and credential values.
    pub fn catalog(
        &self,
        providers: &ProviderRegistry,
        values: &dyn ValueSource,
    ) -> (Vec<rpi_ai::Model>, Vec<String>) {
        provider::catalog_with_diagnostics(self.models.clone(), &providers.manifests, values)
    }
}

#[cfg(test)]
mod test_host;
