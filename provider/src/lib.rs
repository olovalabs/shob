pub mod auth;
pub mod catalog;
pub mod data;
pub mod models;
pub mod providers;
pub mod runtime;
pub mod types;

// Re-export common types and functions for ergonomic access
pub use auth::storage::{disconnect_provider, save_api_key};
pub use auth::{check_all_auth, check_auth};
pub use catalog::{
    CatalogSummary, all_models, all_providers, api_key_providers, default_model_for, get_provider,
    oauth_providers, provider_ids, provider_models, search_models, summary,
};
pub use models::format_model_label;
pub use types::{
    AuthStatus, AuthType, LoginConfig, ModelSpec, ProviderSpec, RefreshConfig, StoredCredential,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_catalog_preserves_core_coverage() {
        let providers = all_providers();
        assert!(providers.iter().any(|provider| provider.id == "otari"));
        assert!(providers.iter().any(|provider| provider.id == "google-antigravity"));
        assert!(providers.iter().any(|provider| provider.id == "factory-droid"));

        let sum = summary();
        assert_eq!(sum.total_providers, providers.len());
        assert!(sum.oauth_providers >= 20, "Should have 20+ OAuth providers");
        assert!(
            sum.api_key_providers >= 60,
            "Should have 60+ API key providers"
        );
        assert!(sum.total_models > 1000, "Should have thousands of models");
    }

    #[test]
    fn test_core_providers_exist() {
        let expected = [
            "openai",
            "openai-codex",
            "anthropic",
            "google",
            "google-antigravity",
            "google-gemini-cli",
            "github-copilot",
            "deepseek",
            "groq",
            "xai",
            "xai-oauth",
            "mistral",
            "openrouter",
            "cerebras",
            "together",
            "cursor",
            "devin",
            "kimi-code",
            "zai",
            "alibaba-coding-plan",
            "xiaomi",
            "minimax",
            "ollama",
        ];

        for id in expected {
            let p = get_provider(id);
            assert!(p.is_some(), "Provider '{id}' must exist in catalog");
        }
    }

    #[test]
    fn test_oauth_providers() {
        let oauth_list = oauth_providers();
        let codex = oauth_list.iter().find(|p| p.id == "openai-codex");
        assert!(codex.is_some());
        let codex = codex.unwrap();
        assert!(codex.login.is_some());
        assert_eq!(codex.login.as_ref().unwrap().kind, "oauth-code");

        let anthropic = oauth_list.iter().find(|p| p.id == "anthropic");
        assert!(anthropic.is_some());
        assert!(anthropic.unwrap().supports_oauth());
    }

    #[test]
    fn test_api_key_resolution() {
        let deepseek = get_provider("deepseek").unwrap();
        assert!(deepseek.supports_api_key());
        assert!(deepseek.env_vars.contains(&"DEEPSEEK_API_KEY".to_string()));
    }

    #[test]
    fn test_model_search() {
        let results = search_models("claude-sonnet");
        assert!(
            !results.is_empty(),
            "Search should find Claude Sonnet models"
        );

        let gpt_results = search_models("gpt-5");
        assert!(!gpt_results.is_empty(), "Search should find GPT-5 models");
    }
}
