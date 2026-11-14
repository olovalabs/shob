use crate::auth::api_key::build_auth_headers;
use crate::catalog::get_provider;
use crate::types::{ModelSpec, ProviderSpec};
use std::collections::HashMap;

/// A generic wrapper around any provider in the compiled catalog.
pub struct GenericProvider {
    pub spec: &'static ProviderSpec,
}

impl GenericProvider {
    pub fn new(provider_id: &str) -> Option<Self> {
        get_provider(provider_id).map(|spec| Self { spec })
    }

    pub fn id(&self) -> &str {
        &self.spec.id
    }

    pub fn name(&self) -> &str {
        &self.spec.name
    }

    pub fn headers(&self, api_key: &str) -> HashMap<String, String> {
        build_auth_headers(self.spec, api_key)
    }

    pub fn models(&self) -> &[ModelSpec] {
        &self.spec.models
    }

    pub fn default_model(&self) -> Option<&str> {
        self.spec.default_model.as_deref()
    }
}
