use std::collections::BTreeMap;
use std::sync::Arc;

use crate::{
    credentials::AuthState, ModelRequest, ProviderRegistry, ResolveError, ResolvedModel,
    RuntimeConfig, ValueSource,
};

/// Full routing identity; display labels are never parsed to identify a model.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ModelIdentity {
    pub provider: String,
    pub api: rpi_ai::Api,
    pub model: String,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelection {
    pub identity: ModelIdentity,
    pub requested_thinking: rpi_ai::ThinkingLevel,
    /// Model-supported preference. Hosts pass this level into stream options;
    /// the provider maps it to its protocol's controls. Non-reasoning models use Off.
    pub effective_thinking: rpi_ai::ThinkingLevel,
}

impl ResolvedModel {
    pub fn selection(&self) -> ModelSelection {
        ModelSelection {
            identity: ModelIdentity {
                provider: self.model.provider.clone(),
                api: self.model.api.clone(),
                model: self.model.id.clone(),
            },
            requested_thinking: self.thinking_level,
            effective_thinking: if self.model.reasoning {
                self.model.clamp_thinking_level(self.thinking_level)
            } else {
                rpi_ai::ThinkingLevel::Off
            },
        }
    }
}

/// Credential-free state suitable for serving to a UI or observing over watch.
#[derive(Clone, Debug, serde::Serialize)]
pub struct RuntimeSnapshot {
    pub revision: u64,
    pub selection: ModelSelection,
    pub models: Vec<rpi_ai::Model>,
    pub auth: BTreeMap<String, AuthState>,
    pub warnings: Vec<String>,
}

/// An optional atomic publication boundary for embedded applications. Hosts
/// continue to own configuration storage, plugin loading and request cancellation.
pub struct Runtime {
    current: Arc<ResolvedModel>,
    changes: tokio::sync::watch::Sender<Arc<RuntimeSnapshot>>,
}

impl Runtime {
    pub fn new(
        config: &RuntimeConfig,
        providers: &ProviderRegistry,
        values: &dyn ValueSource,
        request: &ModelRequest<'_>,
    ) -> Result<Self, ResolveError> {
        let (current, snapshot) = prepare(config, providers, values, request, 0)?;
        let (changes, _) = tokio::sync::watch::channel(Arc::new(snapshot));
        Ok(Self {
            current: Arc::new(current),
            changes,
        })
    }

    /// Cloned providers retain the original loader lease and private bindings.
    pub fn current(&self) -> Arc<ResolvedModel> {
        self.current.clone()
    }

    pub fn snapshot(&self) -> Arc<RuntimeSnapshot> {
        self.changes.borrow().clone()
    }

    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Arc<RuntimeSnapshot>> {
        self.changes.subscribe()
    }

    /// Prepare everything before publishing. Failure leaves both the active
    /// providers and observable snapshot intact, without emitting a change.
    pub fn reload(
        &mut self,
        config: &RuntimeConfig,
        providers: &ProviderRegistry,
        values: &dyn ValueSource,
        request: &ModelRequest<'_>,
    ) -> Result<(), ResolveError> {
        let revision = self.snapshot().revision.saturating_add(1);
        let (current, snapshot) = prepare(config, providers, values, request, revision)?;
        self.current = Arc::new(current);
        self.changes.send_replace(Arc::new(snapshot));
        Ok(())
    }
}

fn prepare(
    config: &RuntimeConfig,
    providers: &ProviderRegistry,
    values: &dyn ValueSource,
    request: &ModelRequest<'_>,
    revision: u64,
) -> Result<(ResolvedModel, RuntimeSnapshot), ResolveError> {
    let current = config.resolve(request, providers, values)?;
    let snapshot = RuntimeSnapshot {
        revision,
        selection: current.selection(),
        models: current.snapshot_catalog.clone(),
        auth: current.snapshot_auth.clone(),
        warnings: current.compat_warnings.clone(),
    };
    Ok((current, snapshot))
}
