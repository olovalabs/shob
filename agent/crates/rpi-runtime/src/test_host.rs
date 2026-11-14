//! File fixtures for the migrated resolver regression tests, never part of the
//! public runtime. Production storage and trust adapters live in rpi-cli.
use crate::{
    ModelRequest, ProviderRegistry, ResolveError, ResolvedModel, RuntimeConfig, RuntimeSettings,
    ValueSource,
};
use rpi_ai::{Model, ThinkingLevel};
use serde::de::DeserializeOwned;
use std::path::PathBuf;

pub struct TestValues;
impl ValueSource for TestValues {
    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

fn read<T: DeserializeOwned + Default>(path: PathBuf) -> Result<T, crate::config::ConfigError> {
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|source| crate::config::ConfigError::Json { path, source }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(source) => Err(crate::config::ConfigError::Read { path, source }),
    }
}
pub mod config {
    pub use crate::config::*;
    pub const CONFIG_DIR_ENV: &str = "RPI_RUNTIME_TEST_CONFIG_DIR";
    pub fn models_path() -> Result<std::path::PathBuf, ConfigError> {
        Ok(std::path::PathBuf::from(std::env::var_os(CONFIG_DIR_ENV).unwrap()).join("models.json"))
    }
    pub fn settings_path() -> Result<std::path::PathBuf, ConfigError> {
        Ok(models_path()?.with_file_name("settings.json"))
    }
    pub fn load_models_config() -> Result<ModelsConfig, ConfigError> {
        super::read(models_path()?)
    }
    pub fn read_auth() -> Result<AuthStore, ConfigError> {
        super::read(models_path()?.with_file_name("auth.json"))
    }
    pub fn upsert_credential(provider: &str, credential: Credential) -> Result<(), ConfigError> {
        let mut store = read_auth()?;
        store.insert(provider.into(), credential);
        let path = models_path()?.with_file_name("auth.json");
        std::fs::write(&path, serde_json::to_vec(&store).unwrap())
            .map_err(|source| ConfigError::Write { path, source })
    }
    pub fn upsert_oauth_token(
        provider: &str,
        access: String,
        refresh: String,
        expires: i64,
    ) -> Result<(), ConfigError> {
        upsert_credential(
            provider,
            Credential::Oauth {
                access,
                refresh,
                expires,
            },
        )
    }
    pub mod test_support {
        pub fn env_lock() -> &'static std::sync::Mutex<()> {
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            &LOCK
        }
    }
}
pub fn resolve(
    provider: Option<&str>,
    model: Option<&str>,
    thinking: Option<ThinkingLevel>,
    api_key: Option<&str>,
    base_url: Option<&str>,
) -> Result<ResolvedModel, ResolveError> {
    RuntimeConfig {
        models: config::load_models_config()?,
        credentials: config::read_auth()?,
        settings: read::<RuntimeSettings>(config::settings_path()?)?,
    }
    .resolve(
        &ModelRequest {
            provider,
            model,
            thinking,
            api_key,
            base_url,
            ..Default::default()
        },
        &ProviderRegistry::default(),
        &TestValues,
    )
}
pub fn catalog_all() -> Result<Vec<Model>, crate::config::ConfigError> {
    catalog_with_diagnostics(&[]).map(|(models, _)| models)
}
pub fn catalog_with_diagnostics(
    manifests: &[crate::ProviderDeclaration],
) -> Result<(Vec<Model>, Vec<String>), crate::config::ConfigError> {
    Ok(crate::provider::catalog_with_diagnostics(
        config::load_models_config()?,
        manifests,
        &TestValues,
    ))
}
pub fn execution_catalog_with_manifests(
    manifests: &[crate::ProviderDeclaration],
) -> Result<Vec<Model>, crate::config::ConfigError> {
    Ok(crate::provider::execution_catalog_with_diagnostics(
        &mut config::load_models_config()?,
        manifests,
        &TestValues,
    )
    .0)
}
