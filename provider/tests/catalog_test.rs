use provider::{
    AuthType, all_models, all_providers, api_key_providers, check_all_auth, check_auth,
    get_provider, oauth_providers, search_models, summary,
};

#[test]
fn test_provider_presence_and_auth_integrity() {
    let providers = all_providers();
    assert!(providers.iter().any(|provider| provider.id == "otari"));
    assert!(providers.iter().any(|provider| provider.id == "xai-oauth"));

    let sum = summary();
    assert_eq!(sum.total_providers, providers.len());
    assert_eq!(sum.oauth_providers, oauth_providers().len());
    assert_eq!(sum.api_key_providers, api_key_providers().len());
    assert!(
        sum.total_models > 5000,
        "Catalog should have 5000+ models across providers, found: {}",
        sum.total_models
    );

    for p in providers {
        assert!(!p.id.is_empty(), "Provider ID must not be empty");
        assert!(!p.name.is_empty(), "Provider name must not be empty");

        match p.auth_type() {
            AuthType::OAuthCode => {
                let login = p
                    .login
                    .as_ref()
                    .expect("OAuthCode provider must have login config");
                assert!(
                    login.client_id.is_some() || p.id == "openrouter" || p.id == "devin",
                    "Provider {} must have client_id",
                    p.id
                );
                assert!(
                    login.authorize_url.is_some(),
                    "Provider {} must have authorize_url",
                    p.id
                );
                assert!(
                    login.token_url.is_some(),
                    "Provider {} must have token_url",
                    p.id
                );
            }
            AuthType::DeviceCode => {
                let login = p
                    .login
                    .as_ref()
                    .expect("DeviceCode provider must have login config");
                assert!(
                    login.client_id.is_some(),
                    "Provider {} must have client_id",
                    p.id
                );
            }
            AuthType::ApiKey => {
                assert!(p.supports_api_key(), "ApiKey provider must support API key");
            }
            AuthType::Custom => {
                let login = p
                    .login
                    .as_ref()
                    .expect("Custom provider must have login config");
                assert!(
                    login.hook.is_some() || login.client_id.is_some(),
                    "Provider {} must have hook or client_id",
                    p.id
                );
            }
            AuthType::None => {}
        }
    }
}

#[test]
fn test_critical_providers_configured_accurately() {
    // Anthropic
    let anthropic = get_provider("anthropic").expect("anthropic must exist");
    assert!(anthropic.name.contains("Anthropic"));
    assert!(anthropic.supports_api_key());
    assert!(
        anthropic
            .env_vars
            .contains(&"ANTHROPIC_API_KEY".to_string())
    );

    // OpenAI & OpenAI Codex
    let openai = get_provider("openai").expect("openai must exist");
    assert!(openai.env_vars.contains(&"OPENAI_API_KEY".to_string()));

    let codex = get_provider("openai-codex").expect("openai-codex must exist");
    assert!(codex.supports_oauth());
    let codex_login = codex.login.as_ref().unwrap();
    assert_eq!(
        codex_login.client_id.as_deref(),
        Some("app_EMoamEEZ73f0CkXaXp7hrann")
    );
    assert_eq!(
        codex_login.authorize_url.as_deref(),
        Some("https://auth.openai.com/oauth/authorize")
    );

    // Google Antigravity & Gemini CLI
    let antigravity = get_provider("google-antigravity").expect("google-antigravity must exist");
    assert!(antigravity.supports_oauth());

    let gemini_cli = get_provider("google-gemini-cli").expect("google-gemini-cli must exist");
    assert!(gemini_cli.supports_oauth());

    // GitHub Copilot
    let copilot = get_provider("github-copilot").expect("github-copilot must exist");
    assert!(copilot.supports_oauth());

    // DeepSeek
    let deepseek = get_provider("deepseek").expect("deepseek must exist");
    assert!(deepseek.supports_api_key());
    assert!(deepseek.env_vars.contains(&"DEEPSEEK_API_KEY".to_string()));

    // Groq
    let groq = get_provider("groq").expect("groq must exist");
    assert!(groq.supports_api_key());
    assert!(groq.env_vars.contains(&"GROQ_API_KEY".to_string()));

    // xAI & xAI OAuth
    let xai = get_provider("xai").expect("xai must exist");
    assert!(xai.supports_api_key());
    let xai_oauth = get_provider("xai-oauth").expect("xai-oauth must exist");
    assert!(xai_oauth.supports_oauth());

    // Cursor
    let cursor = get_provider("cursor").expect("cursor must exist");
    assert!(cursor.supports_oauth());

    // Devin
    let devin = get_provider("devin").expect("devin must exist");
    assert!(devin.supports_oauth());

    // OpenRouter
    let openrouter = get_provider("openrouter").expect("openrouter must exist");
    assert!(openrouter.supports_oauth());
    assert!(openrouter.supports_api_key());
}

#[test]
fn test_check_all_auth_runs_without_panics() {
    let statuses = check_all_auth();
    assert_eq!(statuses.len(), all_providers().len());
    for status in statuses {
        assert!(!status.provider.is_empty());
    }
}

#[test]
fn test_model_catalog_and_search() {
    let all = all_models();
    assert!(all.len() > 5000);

    // Search for claude
    let claude_models = search_models("claude-sonnet");
    assert!(!claude_models.is_empty());

    // Search for gpt
    let gpt_models = search_models("gpt-4");
    assert!(!gpt_models.is_empty());

    // Search for deepseek
    let deepseek_models = search_models("deepseek");
    assert!(!deepseek_models.is_empty());
}

#[test]
fn test_save_and_resolve_api_key() {
    let test_key = "test-sk-1234567890abcdef";
    provider::save_api_key("deepseek", test_key).expect("save_api_key must succeed");

    let status = check_auth("deepseek");
    assert!(status.ready, "deepseek should now be marked ready");
    assert!(status.source.is_some());

    provider::disconnect_provider("deepseek").expect("disconnect Kathamo provider");
    assert!(provider::auth::storage::get_local_credential("deepseek").is_none());
}
