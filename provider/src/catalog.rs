use crate::data::ALL_PROVIDERS;
use crate::types::{ModelSpec, ProviderSpec};
use std::collections::HashMap;
use std::sync::LazyLock;

static PROVIDERS_BY_ID: LazyLock<HashMap<&'static str, &'static ProviderSpec>> =
    LazyLock::new(|| {
        ALL_PROVIDERS
            .iter()
            .map(|spec| (spec.id.as_str(), spec))
            .collect()
    });

/// Get every compiled provider specification, including additive upstream updates.
pub fn all_providers() -> &'static [ProviderSpec] {
    &ALL_PROVIDERS
}

/// Retrieve a specific provider specification by ID (case-insensitive fallback).
pub fn get_provider(id: &str) -> Option<&'static ProviderSpec> {
    if let Some(spec) = PROVIDERS_BY_ID.get(id) {
        return Some(spec);
    }
    // Case-insensitive fallback
    ALL_PROVIDERS
        .iter()
        .find(|spec| spec.id.eq_ignore_ascii_case(id))
}

/// List all provider IDs in catalog order.
pub fn provider_ids() -> Vec<&'static str> {
    ALL_PROVIDERS.iter().map(|spec| spec.id.as_str()).collect()
}

/// Return all providers that support OAuth authentication.
pub fn oauth_providers() -> Vec<&'static ProviderSpec> {
    ALL_PROVIDERS
        .iter()
        .filter(|spec| spec.supports_oauth())
        .collect()
}

/// Return all providers that support API key authentication.
pub fn api_key_providers() -> Vec<&'static ProviderSpec> {
    ALL_PROVIDERS
        .iter()
        .filter(|spec| spec.supports_api_key())
        .collect()
}

/// Return all models for a given provider.
pub fn provider_models(provider_id: &str) -> Option<&'static [ModelSpec]> {
    get_provider(provider_id).map(|p| p.models.as_slice())
}

/// Return the default model ID for a given provider, if defined.
pub fn default_model_for(provider_id: &str) -> Option<&'static str> {
    get_provider(provider_id).and_then(|p| p.default_model.as_deref())
}

/// Return every model across all compiled providers.
pub fn all_models() -> Vec<(&'static ProviderSpec, &'static ModelSpec)> {
    let mut result = Vec::new();
    for provider in ALL_PROVIDERS.iter() {
        for model in &provider.models {
            result.push((provider, model));
        }
    }
    result
}

/// Search models by substring match on ID or name across all providers.
pub fn search_models(query: &str) -> Vec<(&'static ProviderSpec, &'static ModelSpec)> {
    let query_lower = query.trim().to_ascii_lowercase();
    if query_lower.is_empty() {
        return all_models();
    }
    all_models()
        .into_iter()
        .filter(|(provider, model)| {
            provider.id.to_ascii_lowercase().contains(&query_lower)
                || provider.name.to_ascii_lowercase().contains(&query_lower)
                || model.id.to_ascii_lowercase().contains(&query_lower)
                || model.name.to_ascii_lowercase().contains(&query_lower)
        })
        .collect()
}

/// Summary counts of providers by primary authentication category.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogSummary {
    pub total_providers: usize,
    pub oauth_providers: usize,
    pub api_key_providers: usize,
    pub total_models: usize,
}

pub fn summary() -> CatalogSummary {
    let total_providers = ALL_PROVIDERS.len();
    let oauth_providers = oauth_providers().len();
    let api_key_providers = api_key_providers().len();
    let total_models: usize = ALL_PROVIDERS.iter().map(|p| p.models.len()).sum();

    CatalogSummary {
        total_providers,
        oauth_providers,
        api_key_providers,
        total_models,
    }
}
