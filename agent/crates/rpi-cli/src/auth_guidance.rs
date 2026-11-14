//! Auth guidance strings.
//!
//! Port of the upstream `packages/coding-agent/src/core/auth-guidance.ts`.
//! These produce the user-facing "you have no model / no key — here's how to fix
//! it" messages so every entry point (startup, `/model`, provider resolution)
//! says the same thing.

/// Where the login help points. rpi keeps its documentation embedded in the
/// crate, so this is a stable identifier rather than a filesystem path.
pub const DOCS_PROVIDERS: &str = "docs/providers.md";
pub const DOCS_MODELS: &str = "docs/models.md";

/// Native `getProviderLoginHelp`.
pub fn provider_login_help() -> String {
    [
        "Use /login to log into a provider via OAuth or API key. See:",
        &format!("  {DOCS_PROVIDERS}"),
        &format!("  {DOCS_MODELS}"),
    ]
    .join("\n")
}

/// Native `formatNoModelsAvailableMessage`.
pub fn no_models_available_message() -> String {
    format!("No models available. {}", provider_login_help())
}

/// Native `formatNoModelSelectedMessage`.
pub fn no_model_selected_message() -> String {
    format!(
        "No model selected.\n\n{}\n\nThen use /model to select a model.",
        provider_login_help()
    )
}

/// Native `formatNoApiKeyFoundMessage`.
pub fn no_api_key_found_message(provider: &str) -> String {
    let display = if provider.is_empty() || provider == "unknown" {
        "the selected model".to_string()
    } else {
        provider.to_string()
    };
    format!(
        "No API key found for {display}.\n\n{}",
        provider_login_help()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_mention_login() {
        assert!(no_models_available_message().contains("/login"));
        assert!(no_model_selected_message().contains("/model"));
        assert!(no_api_key_found_message("anthropic").contains("anthropic"));
        assert!(no_api_key_found_message("").contains("the selected model"));
    }
}
