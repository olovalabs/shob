//! File, trust and native-extension adapters for the shared model runtime.
use crate::{config, settings};
use rpi_ai::{Model, ThinkingLevel};
use rpi_runtime::provider::RuntimeFactories;
pub use rpi_runtime::provider::{
    authenticated_catalog, available_catalog, ResolveError, ResolvedModel, ANTHROPIC_API_KEY_ENV,
    ANTHROPIC_AUTH_TOKEN_ENV, ANTHROPIC_BASE_URL_ENV, DEFAULT_MODEL_ID, DEFAULT_THINKING_LEVEL,
    NO_API_KEY_HINT, OPENAI_API_KEY_ENV,
};
use std::sync::Arc;

pub(crate) struct CliValues;
impl rpi_runtime::ValueSource for CliValues {
    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
    fn command(&self, command: &str, cached: bool) -> Option<String> {
        if cached {
            config::resolve_command(command)
        } else {
            config::resolve_command_uncached(command)
        }
    }
}
pub(crate) fn declaration(
    manifest: &rpi_extensions::ProviderManifest,
) -> rpi_runtime::ProviderDeclaration {
    rpi_runtime::ProviderDeclaration {
        id: manifest.id.clone(),
        models: manifest.models.clone(),
        api_key_env: manifest.api_key_env.clone(),
        requires_auth: manifest.requires_auth,
    }
}
fn runtime_settings(settings: settings::Settings) -> rpi_runtime::RuntimeSettings {
    let timeout = settings.http_idle_timeout_ms();
    rpi_runtime::RuntimeSettings {
        default_provider: settings.default_provider,
        default_model: settings.default_model,
        default_thinking_level: settings.default_thinking_level,
        theme: settings.theme,
        http_idle_timeout_ms: timeout,
    }
}
fn resolve_with_factories(
    provider: Option<&str>,
    model: Option<&str>,
    thinking: Option<ThinkingLevel>,
    api_key: Option<&str>,
    base_url: Option<&str>,
    settings: settings::Settings,
    factories: &mut RuntimeFactories,
) -> Result<ResolvedModel, ResolveError> {
    let models = config::load_models_config()?;
    let credentials = config::read_auth()?;
    let selected = provider
        .map(str::to_owned)
        .or_else(|| {
            model
                .and_then(|model| model.split_once('/'))
                .filter(|(id, _)| {
                    rpi_runtime::catalog::builtin_providers()
                        .iter()
                        .any(|provider| provider.id.eq_ignore_ascii_case(id))
                })
                .map(|(provider, _)| provider.to_owned())
        })
        .or_else(|| settings.default_provider.clone())
        .and_then(|id| {
            rpi_runtime::catalog::builtin_providers()
                .iter()
                .find(|provider| provider.id.eq_ignore_ascii_case(&id))
                .map(|provider| provider.id.clone())
        });
    let mut discovery_warnings = Vec::new();
    if let Some(selected) = selected.filter(|id| {
        rpi_runtime::catalog::builtin_provider(id).is_some_and(|provider| provider.discovery)
    }) {
        let mut discovery_config = models.clone();
        let mut discovery_credentials = credentials.clone();
        let configured = discovery_config
            .providers
            .entry(selected.clone())
            .or_insert_with(|| {
                serde_json::from_value(serde_json::json!({"models":[]}))
                    .expect("empty provider configuration")
            });
        if let Some(base) = base_url {
            configured.base_url = Some(base.to_owned());
        }
        if let Some(key) = api_key {
            discovery_credentials.insert(
                selected.clone(),
                config::Credential::ApiKey {
                    key: Some(key.to_owned()),
                    env: None,
                },
            );
        }
        let (declarations, warnings) =
            discover_blocking(discovery_config, discovery_credentials, Some(selected));
        discovery_warnings = warnings;
        for declaration in declarations {
            if !factories
                .manifests
                .iter()
                .any(|known| known.id == declaration.id)
            {
                factories.declare(declaration);
            }
        }
    }
    let mut resolved = rpi_runtime::RuntimeConfig {
        models,
        credentials,
        settings: runtime_settings(settings),
    }
    .resolve(
        &rpi_runtime::ModelRequest {
            provider,
            model,
            thinking,
            api_key,
            base_url,
            ..Default::default()
        },
        factories,
        &CliValues,
    )?;
    resolved.compat_warnings.extend(discovery_warnings);
    Ok(resolved)
}
pub fn resolve(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<ThinkingLevel>,
    cli_api_key: Option<&str>,
    cli_base_url: Option<&str>,
) -> Result<ResolvedModel, ResolveError> {
    let settings = settings::load_settings().unwrap_or_default();
    resolve_with_settings(
        cli_provider,
        cli_model,
        cli_thinking,
        cli_api_key,
        cli_base_url,
        settings,
    )
}

/// Resolve using upstream's global -> trusted-project settings precedence.
pub fn resolve_for_cwd(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<ThinkingLevel>,
    cli_api_key: Option<&str>,
    cli_base_url: Option<&str>,
    cwd: &std::path::Path,
    project_trusted: bool,
) -> Result<ResolvedModel, ResolveError> {
    let settings = settings::load_effective_model_settings(cwd, project_trusted)?;
    resolve_with_settings(
        cli_provider,
        cli_model,
        cli_thinking,
        cli_api_key,
        cli_base_url,
        settings,
    )
}

fn resolve_with_settings(
    cli_provider: Option<&str>,
    cli_model: Option<&str>,
    cli_thinking: Option<ThinkingLevel>,
    cli_api_key: Option<&str>,
    cli_base_url: Option<&str>,
    settings: settings::Settings,
) -> Result<ResolvedModel, ResolveError> {
    resolve_with_factories(
        cli_provider,
        cli_model,
        cli_thinking,
        cli_api_key,
        cli_base_url,
        settings,
        &mut runtime_factories(None),
    )
}

pub fn resolve_with_extensions(
    args: &crate::args::Args,
    cwd: &std::path::Path,
    trusted: bool,
    session: &rpi_extensions::ExtensionSession,
) -> Result<ResolvedModel, ResolveError> {
    let settings = settings::load_effective_model_settings(cwd, trusted)?;
    resolve_with_factories(
        args.provider.as_deref(),
        args.model.as_deref(),
        args.thinking,
        args.api_key.as_deref(),
        args.base_url.as_deref(),
        settings,
        &mut runtime_factories(Some(session)),
    )
}

/// Build the same native OAuth/provider registry used by CLI and embedded hosts.
pub fn runtime_factories(session: Option<&rpi_extensions::ExtensionSession>) -> RuntimeFactories {
    let mut factories = RuntimeFactories::default();
    let extension = session
        .and_then(crate::auth::PackageOAuthRefresher::new)
        .map(|refresher| Arc::new(refresher) as Arc<dyn rpi_runtime::credentials::OAuthRefresher>);
    factories.set_oauth_refresher(crate::native_auth::refresher(extension));
    let Some(session) = session else {
        return factories;
    };
    if let Some(snapshot) = session.snapshot() {
        for manifest in manifests_from_session(session) {
            factories.declare(declaration(&manifest));
        }
        for provider in rpi_extensions::PluggableProvider::from_session(
            session,
            tokio::runtime::Handle::current(),
        ) {
            if let Some(record) = snapshot
                .providers()
                .iter()
                .find(|record| record.provider_id == provider.id())
            {
                let api =
                    serde_json::from_value(serde_json::Value::String(record.api_style.clone()))
                        .expect("API strings include custom protocols");
                factories.register(provider, api);
            }
        }
    }
    factories
}
pub fn catalog_all() -> Result<Vec<Model>, config::ConfigError> {
    catalog_with_manifests(&[])
}
pub fn manifests_from_session(
    session: &rpi_extensions::ExtensionSession,
) -> Vec<rpi_extensions::ProviderManifest> {
    session.snapshot().map_or_else(Vec::new, |snapshot| {
        snapshot
            .providers()
            .iter()
            .filter_map(|provider| snapshot.provider_manifest(&provider.provider_id).cloned())
            .collect()
    })
}

pub fn catalog_with_manifests(
    manifests: &[rpi_extensions::ProviderManifest],
) -> Result<Vec<Model>, config::ConfigError> {
    catalog_with_diagnostics(manifests).map(|(models, _)| models)
}
pub(crate) fn catalog_with_diagnostics(
    manifests: &[rpi_extensions::ProviderManifest],
) -> Result<(Vec<Model>, Vec<String>), config::ConfigError> {
    let models = config::load_models_config()?;
    let (mut declarations, mut warnings) =
        discover_blocking(models.clone(), config::read_auth()?, None);
    // Loaded extension declarations remain more specific than native discovery.
    declarations.retain(|discovered| {
        !manifests
            .iter()
            .any(|manifest| manifest.id == discovered.id)
    });
    declarations.extend(manifests.iter().map(declaration));
    let (catalog, diagnostics) =
        rpi_runtime::provider::catalog_with_diagnostics(models, &declarations, &CliValues);
    warnings.extend(diagnostics);
    Ok((catalog, warnings))
}

fn discover_blocking(
    models: config::ModelsConfig,
    credentials: config::AuthStore,
    selected: Option<String>,
) -> (Vec<rpi_runtime::ProviderDeclaration>, Vec<String>) {
    // This adapter is called both outside Tokio and inside an embedded host's
    // executor. A dedicated thread avoids nested-runtime/block_on panics.
    std::thread::spawn(move || {
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime.block_on(rpi_runtime::catalog::discover_models_for(
                &models,
                &credentials,
                &CliValues,
                selected.as_deref(),
            )),
            Err(error) => (
                Vec::new(),
                vec![format!("Cannot initialize model discovery: {error}")],
            ),
        }
    })
    .join()
    .unwrap_or_else(|_| {
        (
            Vec::new(),
            vec!["Model discovery worker panicked".to_owned()],
        )
    })
}

#[cfg(test)]
mod tests;
