//! Model registry runtime - dynamic model registration and refresh.
//!
//! Mirrors the upstream `ModelRegistry` and `ModelRuntime` functionality:
//! - Dynamic provider registration/unregistration
//! - Runtime model catalog refresh
//! - Provider authentication status tracking
//! - Extension provider integration
//! - Real-time catalog change notifications

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;

use rpi_ai::model::Model;
use rpi_ai::Provider;

/// Model registry event types
#[derive(Debug, Clone)]
pub enum ModelRegistryEvent {
    /// A new provider was registered
    ProviderRegistered { provider_id: String },
    /// A provider was unregistered
    ProviderUnregistered { provider_id: String },
    /// Model catalog was refreshed
    CatalogRefreshed,
    /// Provider auth status changed
    AuthStatusChanged {
        provider_id: String,
        authenticated: bool,
    },
}

/// Provider authentication status
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStatus {
    /// Not authenticated
    Unauthenticated,
    /// Authenticated with valid credentials
    Authenticated,
    /// Authentication expired/invalid
    Expired,
    /// Authentication status unknown
    Unknown,
}

/// Provider information in the registry
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    /// Provider ID
    pub id: String,
    /// Display name
    pub name: String,
    /// Whether this is a built-in provider
    pub builtin: bool,
    /// Authentication status
    pub auth_status: AuthStatus,
    /// Available models
    pub models: Vec<Model>,
}

/// Model registry - manages providers and models at runtime
pub struct ModelRegistry {
    /// Registered providers (metadata)
    providers: Arc<RwLock<HashMap<String, ProviderInfo>>>,
    /// Provider instances (for actual API calls)
    provider_instances: Arc<RwLock<HashMap<String, Arc<dyn Provider>>>>,
    /// Event broadcaster
    event_tx: broadcast::Sender<ModelRegistryEvent>,
    /// Models.json last refresh time
    last_refresh: Arc<RwLock<Option<std::time::Instant>>>,
}

impl ModelRegistry {
    /// Create a new model registry
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(100);
        Self {
            providers: Arc::new(RwLock::new(HashMap::new())),
            provider_instances: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            last_refresh: Arc::new(RwLock::new(None)),
        }
    }

    /// Register a built-in provider
    pub fn register_builtin_provider(
        &self,
        id: String,
        name: String,
        models: Vec<Model>,
    ) -> Result<(), RegistryError> {
        let info = ProviderInfo {
            id: id.clone(),
            name,
            builtin: true,
            auth_status: AuthStatus::Unknown,
            models,
        };

        let mut providers = self
            .providers
            .write()
            .map_err(|_| RegistryError::LockPoisoned)?;
        providers.insert(id.clone(), info);

        let _ = self
            .event_tx
            .send(ModelRegistryEvent::ProviderRegistered { provider_id: id });

        Ok(())
    }

    /// Register an extension provider
    pub fn register_extension_provider(
        &self,
        provider: Arc<dyn Provider>,
    ) -> Result<(), RegistryError> {
        let id = provider.id().to_string();
        let models = provider.models().to_vec();

        let info = ProviderInfo {
            id: id.clone(),
            name: id.clone(), // Extension providers use id as name
            builtin: false,
            auth_status: AuthStatus::Unknown,
            models,
        };

        let mut providers = self
            .providers
            .write()
            .map_err(|_| RegistryError::LockPoisoned)?;
        providers.insert(id.clone(), info);

        let mut instances = self
            .provider_instances
            .write()
            .map_err(|_| RegistryError::LockPoisoned)?;
        instances.insert(id.clone(), provider);

        let _ = self
            .event_tx
            .send(ModelRegistryEvent::ProviderRegistered { provider_id: id });

        Ok(())
    }

    /// Unregister a provider
    pub fn unregister_provider(&self, provider_id: &str) -> Result<(), RegistryError> {
        let mut providers = self
            .providers
            .write()
            .map_err(|_| RegistryError::LockPoisoned)?;
        let mut instances = self
            .provider_instances
            .write()
            .map_err(|_| RegistryError::LockPoisoned)?;

        if providers.remove(provider_id).is_some() {
            instances.remove(provider_id);
            let _ = self
                .event_tx
                .send(ModelRegistryEvent::ProviderUnregistered {
                    provider_id: provider_id.to_string(),
                });
            Ok(())
        } else {
            Err(RegistryError::ProviderNotFound(provider_id.to_string()))
        }
    }

    /// Get provider info
    pub fn get_provider(&self, provider_id: &str) -> Option<ProviderInfo> {
        let providers = self.providers.read().ok()?;
        providers.get(provider_id).cloned()
    }

    /// Get all providers
    pub fn list_providers(&self) -> Vec<ProviderInfo> {
        let providers = self
            .providers
            .read()
            .ok()
            .map(|p| p.values().cloned().collect());
        providers.unwrap_or_default()
    }

    /// Get all models across all providers
    pub fn list_models(&self) -> Vec<Model> {
        let providers = match self.providers.read() {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        };

        providers
            .values()
            .flat_map(|info| info.models.clone())
            .collect()
    }

    /// Get models for a specific provider
    pub fn list_provider_models(&self, provider_id: &str) -> Vec<Model> {
        let providers = match self.providers.read() {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        };

        providers
            .get(provider_id)
            .map(|info| info.models.clone())
            .unwrap_or_default()
    }

    /// Update provider authentication status
    pub fn update_auth_status(
        &self,
        provider_id: &str,
        status: AuthStatus,
    ) -> Result<(), RegistryError> {
        let mut providers = self
            .providers
            .write()
            .map_err(|_| RegistryError::LockPoisoned)?;

        if let Some(info) = providers.get_mut(provider_id) {
            info.auth_status = status.clone();

            let authenticated = matches!(status, AuthStatus::Authenticated);
            let _ = self.event_tx.send(ModelRegistryEvent::AuthStatusChanged {
                provider_id: provider_id.to_string(),
                authenticated,
            });

            Ok(())
        } else {
            Err(RegistryError::ProviderNotFound(provider_id.to_string()))
        }
    }

    /// Get provider authentication status
    pub fn get_auth_status(&self, provider_id: &str) -> Option<AuthStatus> {
        let providers = self.providers.read().ok()?;
        providers
            .get(provider_id)
            .map(|info| info.auth_status.clone())
    }

    /// Refresh model catalog from models.json
    pub async fn refresh_catalog(&self) -> Result<(), RegistryError> {
        // Read models.json from config directory
        let config = crate::config::load_models_config()
            .map_err(|e| RegistryError::InvalidConfiguration(e.to_string()))?;

        // Parse and validate models
        let mut providers = self
            .providers
            .write()
            .map_err(|_| RegistryError::LockPoisoned)?;

        // Update existing providers and register providers that appear only in
        // models.json (native `models.json` custom providers). Previously only
        // pre-registered providers were updated, so a user-defined provider was
        // silently ignored.
        for (provider_id, provider_config) in &config.providers {
            if let Some(models) = crate::config::provider_to_models(provider_id, provider_config) {
                if let Some(info) = providers.get_mut(provider_id) {
                    info.models = models;
                    continue;
                }
            }
            // New provider: register it from models.json.
            let models =
                crate::config::provider_to_models(provider_id, provider_config).unwrap_or_default();
            providers.insert(
                provider_id.clone(),
                ProviderInfo {
                    id: provider_id.clone(),
                    name: provider_config
                        .name
                        .clone()
                        .unwrap_or_else(|| provider_id.clone()),
                    builtin: false,
                    auth_status: AuthStatus::Unknown,
                    models,
                },
            );
        }

        drop(providers);

        // Update last refresh time
        let mut last_refresh = self
            .last_refresh
            .write()
            .map_err(|_| RegistryError::LockPoisoned)?;
        *last_refresh = Some(std::time::Instant::now());

        // Broadcast CatalogRefreshed event
        let _ = self.event_tx.send(ModelRegistryEvent::CatalogRefreshed);

        Ok(())
    }

    /// Get last catalog refresh time
    pub fn last_refresh_time(&self) -> Option<std::time::Instant> {
        self.last_refresh.read().ok().and_then(|t| *t)
    }

    /// Subscribe to registry events
    pub fn subscribe(&self) -> broadcast::Receiver<ModelRegistryEvent> {
        self.event_tx.subscribe()
    }

    /// Get provider instance for API calls
    pub fn get_provider_instance(&self, provider_id: &str) -> Option<Arc<dyn Provider>> {
        let instances = self.provider_instances.read().ok()?;
        instances.get(provider_id).cloned()
    }

    /// Check if a provider is registered
    pub fn has_provider(&self, provider_id: &str) -> bool {
        let providers = match self.providers.read() {
            Ok(p) => p,
            Err(_) => return false,
        };
        providers.contains_key(provider_id)
    }

    /// Get provider count
    pub fn provider_count(&self) -> usize {
        self.providers.read().map(|p| p.len()).unwrap_or(0)
    }

    /// Get model count
    pub fn model_count(&self) -> usize {
        let providers = match self.providers.read() {
            Ok(p) => p,
            Err(_) => return 0,
        };
        providers.values().map(|info| info.models.len()).sum()
    }
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Registry errors
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("Lock poisoned")]
    LockPoisoned,

    #[error("Provider not found: {0}")]
    ProviderNotFound(String),

    #[error("Provider already registered: {0}")]
    ProviderAlreadyRegistered(String),

    #[error("Invalid provider configuration: {0}")]
    InvalidConfiguration(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_creation() {
        let registry = ModelRegistry::new();
        assert_eq!(registry.provider_count(), 0);
        assert_eq!(registry.model_count(), 0);
    }

    #[test]
    fn test_provider_registration() {
        let registry = ModelRegistry::new();

        assert!(registry
            .register_builtin_provider("test".to_string(), "Test Provider".to_string(), vec![])
            .is_ok());
        assert_eq!(registry.provider_count(), 1);
        assert!(registry.has_provider("test"));
    }

    #[test]
    fn test_provider_unregistration() {
        let registry = ModelRegistry::new();

        registry
            .register_builtin_provider("test".to_string(), "Test Provider".to_string(), vec![])
            .unwrap();
        assert!(registry.unregister_provider("test").is_ok());
        assert_eq!(registry.provider_count(), 0);
    }

    #[test]
    fn test_auth_status_update() {
        let registry = ModelRegistry::new();

        registry
            .register_builtin_provider("test".to_string(), "Test Provider".to_string(), vec![])
            .unwrap();

        assert!(registry
            .update_auth_status("test", AuthStatus::Authenticated)
            .is_ok());
        assert_eq!(
            registry.get_auth_status("test"),
            Some(AuthStatus::Authenticated)
        );
    }
}
