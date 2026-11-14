use crate::catalog::{all_models, get_provider};
use crate::types::ModelSpec;

/// Find a specific model by provider ID and model ID.
pub fn find_model(provider_id: &str, model_id: &str) -> Option<&'static ModelSpec> {
    let spec = get_provider(provider_id)?;
    spec.models.iter().find(|m| m.id == model_id)
}

/// Retrieve all models that support reasoning / thinking effort.
pub fn reasoning_models() -> Vec<(String, &'static ModelSpec)> {
    all_models()
        .into_iter()
        .filter(|(_, m)| m.reasoning)
        .map(|(p, m)| (p.id.clone(), m))
        .collect()
}

/// Retrieve all models that accept multimodal / image input.
pub fn multimodal_models() -> Vec<(String, &'static ModelSpec)> {
    all_models()
        .into_iter()
        .filter(|(_, m)| m.input.iter().any(|i| i == "image"))
        .map(|(p, m)| (p.id.clone(), m))
        .collect()
}

/// Format a model for human-readable agent display (e.g. "anthropic · claude-3-7-sonnet").
pub fn format_model_label(provider_id: &str, model_id: &str) -> String {
    format!("{provider_id} · {model_id}")
}
