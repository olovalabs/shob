use super::*;
use crate::test_host::config::test_support::env_lock;
use crate::test_host::{
    catalog_all, catalog_with_diagnostics, config, execution_catalog_with_manifests, resolve,
};
use crate::thinking::{parse_thinking_level, VALID_THINKING_LEVELS};

#[test]
fn catalog_and_resolution_return_compat_diagnostics_without_writing_stderr() {
    const CHILD: &str = "RPI_TEST_QUIET_COMPAT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "provider::tests::catalog_and_resolution_return_compat_diagnostics_without_writing_stderr", "--nocapture"])
                .env(CHILD, "1")
                .output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "TUI catalog queries must stay quiet: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{"providers":{"fixture":{
            "api":"openai-completions","apiKey":"fixture-only",
            "models":[{"id":"test","compat":{"misspelledFlag":true}}]
        }}}"#,
    )
    .unwrap();
    let resolved = resolve(Some("fixture"), Some("test"), None, None, None).unwrap();
    assert_eq!(resolved.compat_warnings.len(), 1);
    assert!(resolved.compat_warnings[0].contains("misspelledFlag"));
    let (_, warnings) = catalog_with_diagnostics(&[]).unwrap();
    assert_eq!(warnings, resolved.compat_warnings);
    assert!(catalog_all()
        .unwrap()
        .iter()
        .any(|model| model.provider == "fixture"));
    assert!(execution_catalog_with_manifests(&[])
        .unwrap()
        .iter()
        .any(|model| model.provider == "fixture"));
}

#[test]
fn extension_api_defaults_are_resolved_before_compat_diagnostics() {
    let mut cfg: config::ModelsConfig = serde_json::from_value(serde_json::json!({
        "providers": {"proxy": {"models": [{"id":"custom", "compat": {
            "thinkingFormat":"deepseek", "requiresReasoningContentOnAssistantMessages":true,
            "misspelledFlag":true
        }}]}}
    }))
    .unwrap();
    let model = Model::new(
        "default",
        "Default",
        rpi_ai::Api::OpenaiCompletions,
        "proxy",
        "http://127.0.0.1/v1",
    );
    apply_manifest_defaults(
        &mut cfg,
        &[crate::ProviderDeclaration {
            id: "proxy".into(),
            models: vec![model],
            api_key_env: None,
            requires_auth: false,
        }],
    );
    let warnings = config::model_compat_warnings(&cfg);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("misspelledFlag"));
    let models = config::provider_to_models(
        "proxy",
        &cfg.providers["proxy"],
        &crate::test_host::TestValues,
    )
    .unwrap();
    let Some(rpi_ai::StreamingProtocolCompat::OpenaiCompletions(compat)) = &models[0].compat else {
        panic!("extension protocol must retain model overrides");
    };
    assert_eq!(compat.requires_reasoning_content, Some(true));
}

#[test]
fn anthropic_messages_models_keep_their_compat() {
    let cfg: config::ModelsConfig = serde_json::from_value(serde_json::json!({
        "providers": {"gateway": {"api": "anthropic-messages", "models": [{"id": "m", "compat": {
            "forceAdaptiveThinking": true, "misspelledFlag": true
        }}]}}
    }))
    .unwrap();
    let warnings = config::model_compat_warnings(&cfg);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("misspelledFlag"));
    let models = config::provider_to_models(
        "gateway",
        &cfg.providers["gateway"],
        &crate::test_host::TestValues,
    )
    .unwrap();
    let Some(rpi_ai::StreamingProtocolCompat::AnthropicMessages(compat)) = &models[0].compat else {
        panic!("anthropic-messages compat must reach the model");
    };
    assert_eq!(compat.force_adaptive_thinking, Some(true));
}

/// Scope a test to a throwaway config dir + clear the `ANTHROPIC_*` env
/// vars, restoring both on drop. Holds the shared env lock for its whole
/// lifetime so parallel env-mutating tests across config/provider/auth all
/// serialize on one mutex.
struct TestEnv {
    _guard: std::sync::MutexGuard<'static, ()>,
    prev_key: Option<std::ffi::OsString>,
    prev_tok: Option<std::ffi::OsString>,
    prev_base: Option<std::ffi::OsString>,
    prev_openai_key: Option<std::ffi::OsString>,
    prev_dir: Option<std::ffi::OsString>,
    _tmp: tempfile::TempDir,
}
impl TestEnv {
    fn new() -> Self {
        let guard = env_lock().lock().unwrap();
        let prev_key = std::env::var_os(ANTHROPIC_API_KEY_ENV);
        let prev_tok = std::env::var_os(ANTHROPIC_AUTH_TOKEN_ENV);
        let prev_base = std::env::var_os(ANTHROPIC_BASE_URL_ENV);
        let prev_openai_key = std::env::var_os(OPENAI_API_KEY_ENV);
        let prev_dir = std::env::var_os(config::CONFIG_DIR_ENV);
        std::env::remove_var(ANTHROPIC_API_KEY_ENV);
        std::env::remove_var(ANTHROPIC_AUTH_TOKEN_ENV);
        std::env::remove_var(ANTHROPIC_BASE_URL_ENV);
        std::env::remove_var(OPENAI_API_KEY_ENV);
        let tmp = tempfile::TempDir::new().unwrap();
        std::env::set_var(config::CONFIG_DIR_ENV, tmp.path());
        Self {
            _guard: guard,
            prev_key,
            prev_tok,
            prev_base,
            prev_openai_key,
            prev_dir,
            _tmp: tmp,
        }
    }
}
impl Drop for TestEnv {
    fn drop(&mut self) {
        restore(ANTHROPIC_API_KEY_ENV, self.prev_key.take());
        restore(ANTHROPIC_AUTH_TOKEN_ENV, self.prev_tok.take());
        restore(ANTHROPIC_BASE_URL_ENV, self.prev_base.take());
        restore(OPENAI_API_KEY_ENV, self.prev_openai_key.take());
        restore(config::CONFIG_DIR_ENV, self.prev_dir.take());
    }
}
fn restore(name: &str, prev: Option<std::ffi::OsString>) {
    match prev {
        Some(v) => std::env::set_var(name, v),
        None => std::env::remove_var(name),
    }
}

// These tests hit the network-free resolution path only (provider/model
// selection). They set a throwaway credential so `resolve` clears the
// `NoApiKey` gate, then assert the model + thinking choice — never making
// a real request.

fn resolve_with_key(
    provider: Option<&str>,
    model: Option<&str>,
    thinking: Option<ThinkingLevel>,
) -> Result<ResolvedModel, ResolveError> {
    let _env = TestEnv::new();
    std::env::set_var(ANTHROPIC_API_KEY_ENV, "test-key");
    resolve(provider, model, thinking, None, None)
}

#[test]
fn default_model_matches_native_anthropic_default() {
    let r = resolve_with_key(None, None, None).unwrap();
    assert_eq!(r.model.id, DEFAULT_MODEL_ID);
    assert_eq!(r.thinking_level, DEFAULT_THINKING_LEVEL);
    assert_eq!(r.provider.id(), "anthropic");
}

#[test]
fn cli_endpoint_override_does_not_redirect_other_authenticated_providers() {
    let _env = TestEnv::new();
    std::fs::write(config::models_path().unwrap(), r#"{"providers":{
            "first":{"api":"openai-completions","apiKey":"first-key","baseUrl":"https://first.example/v1","models":[{"id":"one"}]},
            "second":{"api":"openai-completions","apiKey":"second-key","baseUrl":"https://second.example/v1","models":[{"id":"two"}]}
        }}"#).unwrap();
    let resolved = resolve(
        Some("first"),
        Some("one"),
        None,
        None,
        Some("http://localhost:8080/v1"),
    )
    .unwrap();
    assert_eq!(resolved.model.base_url, "http://localhost:8080/v1");
    let catalog = authenticated_catalog(&resolved);
    let second = catalog
        .iter()
        .find(|model| model.provider == "second")
        .unwrap();
    assert_eq!(second.base_url, "https://second.example/v1");
    assert!(
        second.headers.is_none(),
        "catalogs never expose credentials"
    );
}

#[test]
fn header_only_auth_does_not_expose_unauthenticated_sibling_models() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{"providers":{"gateway":{
            "api":"openai-completions","models":[
                {"id":"authenticated","headers":{"Authorization":"Bearer key"}},
                {"id":"missing"},{"id":"empty","headers":{"Authorization":""}}
            ]
        }}}"#,
    )
    .unwrap();
    let resolved = resolve(Some("gateway"), Some("authenticated"), None, None, None).unwrap();
    let ids: Vec<_> = authenticated_catalog(&resolved)
        .into_iter()
        .map(|model| model.id)
        .collect();
    assert_eq!(ids, ["authenticated"]);
    assert!(resolve(Some("gateway"), Some("empty"), None, None, None).is_err());
}

#[test]
fn every_published_catalog_omits_header_auth_but_requests_recover_it() {
    let _env = TestEnv::new();
    std::fs::write(config::models_path().unwrap(), r#"{"providers":{"gateway":{
            "api":"openai-completions","headers":{"Authorization":"Bearer catalog-secret","X-Private-Header":"private-value"},
            "models":[{"id":"model"}]
        }}}"#).unwrap();
    let resolved = resolve(Some("gateway"), Some("model"), None, None, None).unwrap();
    for catalog in [
        catalog_all().unwrap(),
        available_catalog(&resolved),
        authenticated_catalog(&resolved),
    ] {
        assert!(catalog.iter().all(|model| model.headers.is_none()));
        let json = serde_json::to_string(&catalog).unwrap();
        assert!(!json.contains("catalog-secret") && !json.contains("private-value"));
    }
    assert!(resolved.model.headers.is_none());
    let request = resolved.execution.request_model(&resolved.model);
    assert_eq!(
        request.headers.as_ref().unwrap()["Authorization"],
        "Bearer catalog-secret"
    );
    assert_eq!(
        request.headers.as_ref().unwrap()["X-Private-Header"],
        "private-value"
    );
}

#[test]
fn stored_oauth_uses_bearer_and_expired_tokens_do_not_authenticate_requests() {
    let _env = TestEnv::new();
    config::upsert_oauth_token("anthropic", "access-token".into(), "refresh".into(), 0).unwrap();
    let resolved = resolve(Some("anthropic"), None, None, None, None).unwrap();
    assert_eq!(
        resolved
            .execution
            .request_model(&resolved.model)
            .headers
            .as_ref()
            .unwrap()["authorization"],
        "Bearer access-token"
    );
    config::upsert_oauth_token("anthropic", "expired-token".into(), "refresh".into(), 1).unwrap();
    assert!(resolve(Some("anthropic"), None, None, None, None).is_err());
}

#[test]
fn settings_default_model_wins_when_authed() {
    // A copied pi `settings.json` carrying `defaultModel` (step 3 of pi's
    // `findInitialModel`) overrides the built-in Anthropic default
    // when that model is in the catalog and authed. Mirrors the on-disk-
    // Drop an agent dir at `~/.rpi/agent/` and the saved
    // default comes alive on launch (no `--model` needed).
    let _env = TestEnv::new();
    std::env::set_var(ANTHROPIC_API_KEY_ENV, "k");
    let path = config::settings_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
            &path,
            r#"{"defaultProvider":"anthropic","defaultModel":"claude-haiku-4-5","defaultThinkingLevel":"high"}"#,
        )
        .unwrap();
    let r = resolve(None, None, None, None, None).unwrap();
    assert_eq!(r.model.id, "claude-haiku-4-5");
    assert_eq!(r.thinking_level, ThinkingLevel::High);
    // An unauthed saved default (unknown id) falls through to the built-in.
    std::fs::write(&path, r#"{"defaultModel":"claude-does-not-exist"}"#).unwrap();
    let r = resolve(None, None, None, None, None).unwrap();
    assert_eq!(r.model.id, DEFAULT_MODEL_ID);
}

#[test]
fn explicit_id_match() {
    let r = resolve_with_key(None, Some("claude-haiku-4-5"), None).unwrap();
    assert_eq!(r.model.id, "claude-haiku-4-5");
}

#[test]
fn case_insensitive_id() {
    let r = resolve_with_key(None, Some("CLAUDE-OPUS-5"), None).unwrap();
    assert_eq!(r.model.id, "claude-opus-5");
}

#[test]
fn provider_prefix_stripped() {
    let r = resolve_with_key(None, Some("anthropic/claude-sonnet-5"), None).unwrap();
    assert_eq!(r.model.id, "claude-sonnet-5");
}

#[test]
fn custom_provider_prefix_stripped() {
    // `gateway/custom-claude` resolves to the catalog id `custom-claude`
    // after the `foo/` prefix is stripped.
    let _env = TestEnv::new();
    std::env::set_var(ANTHROPIC_API_KEY_ENV, "official-key");
    std::fs::write(
            config::models_path().unwrap(),
            r#"{ "providers": { "gateway": { "baseUrl": "https://gw", "apiKey": "gateway-key", "models": [{"id":"custom-claude"}] } } }"#,
        )
        .unwrap();
    let r = resolve(None, Some("gateway/custom-claude"), None, None, None).unwrap();
    assert_eq!(r.model.id, "custom-claude");
}

#[test]
fn unknown_slash_prefix_remains_part_of_the_raw_model_id() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "gateway": {
      "api": "openai-completions",
      "apiKey": "gateway-key",
      "models": [{"id":"meta-llama/llama-3.3"}]
    }
  }
}"#,
    )
    .unwrap();

    let resolved = resolve(None, Some("meta-llama/llama-3.3:high"), None, None, None).unwrap();
    assert_eq!(resolved.model.provider, "gateway");
    assert_eq!(resolved.model.id, "meta-llama/llama-3.3");
    assert_eq!(resolved.thinking_level, ThinkingLevel::High);
}

#[test]
fn complete_model_id_wins_before_parsing_a_thinking_suffix() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "gateway": {
      "api": "openai-completions",
      "apiKey": "gateway-key",
      "models": [
        {"id":"vendor/model"},
        {"id":"vendor/model:high"}
      ]
    }
  }
}"#,
    )
    .unwrap();

    let resolved = resolve(None, Some("gateway/vendor/model:high"), None, None, None).unwrap();
    assert_eq!(resolved.model.provider, "gateway");
    assert_eq!(resolved.model.id, "vendor/model:high");
    assert_eq!(resolved.thinking_level, DEFAULT_THINKING_LEVEL);
}

#[test]
fn authenticated_inferred_provider_beats_a_matching_raw_model_id() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "baseUrl": "https://alpha.example.com",
      "apiKey": "alpha-key",
      "models": [{"id":"target"}]
    },
    "gateway": {
      "api": "openai-completions",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "gateway-key",
      "models": [{"id":"alpha/target"}]
    }
  }
}"#,
    )
    .unwrap();

    let resolved = resolve(None, Some("alpha/target"), None, None, None).unwrap();
    assert_eq!(resolved.model.provider, "alpha");
    assert_eq!(resolved.model.id, "target");
    assert_eq!(resolved.model.base_url, "https://alpha.example.com");
}

#[test]
fn authenticated_raw_model_beats_an_unauthenticated_inferred_provider() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "baseUrl": "https://alpha.example.com",
      "models": [{"id":"target"}]
    },
    "gateway": {
      "api": "openai-completions",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "gateway-key",
      "models": [{"id":"alpha/target"}]
    }
  }
}"#,
    )
    .unwrap();

    let resolved = resolve(None, Some("alpha/target"), None, None, None).unwrap();
    assert_eq!(resolved.model.provider, "gateway");
    assert_eq!(resolved.model.id, "alpha/target");
    assert_eq!(resolved.model.base_url, "https://gateway.example.com");
}

#[test]
fn thinking_suffix_raw_model_beats_an_unauthenticated_inferred_provider() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "baseUrl": "https://alpha.example.com",
      "models": [{"id":"target"}]
    },
    "gateway": {
      "api": "openai-completions",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "gateway-key",
      "models": [{"id":"alpha/target"}]
    }
  }
}"#,
    )
    .unwrap();

    let resolved = resolve(None, Some("alpha/target:high"), None, None, None).unwrap();
    assert_eq!(resolved.model.provider, "gateway");
    assert_eq!(resolved.model.id, "alpha/target");
    assert_eq!(resolved.model.base_url, "https://gateway.example.com");
    assert_eq!(resolved.thinking_level, ThinkingLevel::High);
}

#[test]
fn thinking_suffix_in_model() {
    let r = resolve_with_key(None, Some("claude-sonnet-5:high"), None).unwrap();
    assert_eq!(r.model.id, "claude-sonnet-5");
    assert_eq!(r.thinking_level, ThinkingLevel::High);
}

#[test]
fn thinking_flag_overrides_suffix() {
    // `--thinking low` wins over a `:high` suffix.
    let r = resolve_with_key(None, Some("claude-sonnet-5:high"), Some(ThinkingLevel::Low)).unwrap();
    assert_eq!(r.thinking_level, ThinkingLevel::Low);
}

#[test]
fn explicit_provider_anthropic_ok() {
    let r = resolve_with_key(Some("anthropic"), Some("claude-sonnet-5"), None).unwrap();
    assert_eq!(r.model.id, "claude-sonnet-5");

    let r = resolve_with_key(Some("ANTHROPIC"), Some("claude-sonnet-5"), None).unwrap();
    assert_eq!(r.provider.id(), DEFAULT_PROVIDER_ID);
}

#[test]
fn unknown_provider_rejected() {
    let err = resolve_with_key(Some("unsupported-provider"), None, None).unwrap_err();
    assert!(matches!(err, ResolveError::UnknownProvider(_)));
}

#[test]
fn no_match_lists_available() {
    let err = resolve_with_key(None, Some("claude-does-not-exist"), None).unwrap_err();
    match err {
        ResolveError::NoMatch { pattern, available } => {
            assert_eq!(pattern, "claude-does-not-exist");
            assert!(available.contains("claude-sonnet-5"));
        }
        other => panic!("expected NoMatch, got {other:?}"),
    }
}

#[test]
fn colon_not_a_thinking_level_kept_in_id() {
    // A trailing `:foo` that isn't a thinking level stays part of the id
    // pattern → no match (no model id contains `:foo`).
    let err = resolve_with_key(None, Some("claude-sonnet-5:foo"), None).unwrap_err();
    assert!(matches!(err, ResolveError::NoMatch { .. }));
}

#[test]
fn parse_thinking_level_roundtrip() {
    assert_eq!(parse_thinking_level("xhigh"), Some(ThinkingLevel::Xhigh));
    assert_eq!(parse_thinking_level("bogus"), None);
    // Sanity: the valid set matches what help advertises.
    for lvl in VALID_THINKING_LEVELS {
        assert!(parse_thinking_level(lvl).is_some(), "{lvl} should parse");
    }
}

#[test]
fn no_api_key_errors_with_hint() {
    let _env = TestEnv::new();
    let err = resolve(None, None, None, None, None).unwrap_err();
    match err {
        ResolveError::NoApiKey { hint } => {
            assert!(hint.contains("ANTHROPIC_API_KEY"));
            assert!(hint.contains("auth login"));
        }
        other => panic!("expected NoApiKey, got {other:?}"),
    }
}

#[test]
fn stored_credential_satisfies_auth() {
    let _env = TestEnv::new();
    config::upsert_credential(
        DEFAULT_PROVIDER_ID,
        Credential::ApiKey {
            key: Some("stored-key".into()),
            env: None,
        },
    )
    .unwrap();
    let r = resolve(None, None, None, None, None).unwrap();
    assert_eq!(r.model.id, DEFAULT_MODEL_ID);
    // x-api-key path: no Bearer header folded onto the model (auth rides on
    // the provider's default key, surfaced to the provider at build time).
    assert!(
        r.execution
            .request_model(&r.model)
            .headers
            .as_ref()
            .and_then(|h| h.get("authorization"))
            .is_none(),
        "x-api-key path should not synthesize a Bearer header"
    );
}

#[test]
fn gateway_models_key_wins_over_unrelated_anthropic_auth() {
    let _env = TestEnv::new();
    config::upsert_credential(
        DEFAULT_PROVIDER_ID,
        Credential::ApiKey {
            key: Some("official-key".into()),
            env: None,
        },
    )
    .unwrap();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "gateway-key",
      "models": [{"id":"gateway-model"}]
    }
  }
}"#,
    )
    .unwrap();

    let resolved = resolve(None, Some("gateway/gateway-model"), None, None, None).unwrap();
    assert_eq!(resolved.provider.id(), "gateway");
    assert_eq!(resolved.model.provider, "gateway");
    assert!(resolved.has_provider_key);

    let cfg = config::load_models_config().unwrap();
    let credentials = resolve_anthropic_credentials(
        &cfg,
        &config::read_auth().unwrap(),
        &[],
        &crate::test_host::TestValues,
    );
    assert_eq!(
        anthropic_credential_for(&credentials, DEFAULT_PROVIDER_ID),
        Some(&AnthropicCredential::ProviderKey("official-key".into()))
    );
    assert_eq!(
        anthropic_credential_for(&credentials, "gateway"),
        Some(&AnthropicCredential::ProviderKey("gateway-key".into()))
    );
}

#[test]
fn gateway_auth_json_credential_is_usable() {
    let _env = TestEnv::new();
    config::upsert_credential(
        "gateway",
        Credential::ApiKey {
            key: Some("stored-gateway-key".into()),
            env: None,
        },
    )
    .unwrap();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "models": [{"id":"gateway-model"}]
    }
  }
}"#,
    )
    .unwrap();

    let resolved = resolve(Some("gateway"), None, None, None, None).unwrap();
    assert_eq!(resolved.model.id, "gateway-model");
    assert_eq!(resolved.provider.id(), "gateway");
    assert!(resolved.has_provider_key);
}

#[test]
fn anthropic_auth_does_not_authenticate_an_unkeyed_gateway() {
    let _env = TestEnv::new();
    config::upsert_credential(
        DEFAULT_PROVIDER_ID,
        Credential::ApiKey {
            key: Some("official-key".into()),
            env: None,
        },
    )
    .unwrap();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "models": [{"id":"gateway-model"}]
    }
  }
}"#,
    )
    .unwrap();

    let error = resolve(
        Some("gateway"),
        Some("gateway/gateway-model"),
        None,
        None,
        None,
    )
    .unwrap_err();
    assert!(matches!(error, ResolveError::NoApiKey { .. }));

    let default = resolve(None, None, None, None, None).unwrap();
    assert_eq!(default.provider.id(), DEFAULT_PROVIDER_ID);
    assert_eq!(default.model.id, DEFAULT_MODEL_ID);
}

#[test]
fn two_anthropic_gateways_keep_credentials_isolated_by_id() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "alpha": {
      "api": "anthropic-messages",
      "baseUrl": "https://shared.example.com",
      "apiKey": "alpha-key",
      "models": [{"id":"shared-model"}]
    },
    "beta": {
      "api": "anthropic-messages",
      "baseUrl": "https://shared.example.com",
      "apiKey": "beta-key",
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
    )
    .unwrap();

    let cfg = config::load_models_config().unwrap();
    let credentials = resolve_anthropic_credentials(
        &cfg,
        &config::read_auth().unwrap(),
        &[],
        &crate::test_host::TestValues,
    );
    assert_eq!(
        anthropic_credential_for(&credentials, "alpha"),
        Some(&AnthropicCredential::ProviderKey("alpha-key".into()))
    );
    assert_eq!(
        anthropic_credential_for(&credentials, "beta"),
        Some(&AnthropicCredential::ProviderKey("beta-key".into()))
    );
    let alpha = resolve(None, Some("alpha/shared-model"), None, None, None).unwrap();
    let beta = resolve(None, Some("beta/shared-model"), None, None, None).unwrap();
    assert_eq!(alpha.provider.id(), "alpha");
    assert_eq!(beta.provider.id(), "beta");
    assert!(alpha.has_provider_key && beta.has_provider_key);
}

#[test]
fn case_distinct_provider_ids_keep_endpoints_and_credentials_isolated() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "alpha": {
      "api": "anthropic-messages",
      "baseUrl": "https://lower.example.com",
      "authHeader": true,
      "models": [{"id":"shared-model"}]
    },
    "ALPHA": {
      "api": "anthropic-messages",
      "baseUrl": "https://upper.example.com",
      "authHeader": true,
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
    )
    .unwrap();
    config::upsert_credential(
        "alpha",
        Credential::ApiKey {
            key: Some("lower-key".into()),
            env: None,
        },
    )
    .unwrap();
    config::upsert_credential(
        "ALPHA",
        Credential::ApiKey {
            key: Some("upper-key".into()),
            env: None,
        },
    )
    .unwrap();

    let lower = resolve(None, Some("alpha/shared-model"), None, None, None).unwrap();
    assert_eq!(lower.provider.id(), "alpha");
    assert_eq!(lower.model.base_url, "https://lower.example.com");
    assert_eq!(
        lower
            .execution
            .request_model(&lower.model)
            .headers
            .as_ref()
            .and_then(|headers| headers.get("authorization"))
            .map(String::as_str),
        Some("Bearer lower-key")
    );

    let upper = resolve(None, Some("ALPHA/shared-model"), None, None, None).unwrap();
    assert_eq!(upper.provider.id(), "ALPHA");
    assert_eq!(upper.model.base_url, "https://upper.example.com");
    assert_eq!(
        upper
            .execution
            .request_model(&upper.model)
            .headers
            .as_ref()
            .and_then(|headers| headers.get("authorization"))
            .map(String::as_str),
        Some("Bearer upper-key")
    );

    let error = resolve(Some("Alpha"), None, None, None, None).unwrap_err();
    assert!(matches!(
        error,
        ResolveError::AmbiguousProvider {
            requested,
            matches
        } if requested == "Alpha" && matches == "ALPHA, alpha"
    ));

    let prefix_error = resolve(None, Some("Alpha/shared-model"), None, None, None).unwrap_err();
    assert!(matches!(
        prefix_error,
        ResolveError::AmbiguousProvider {
            requested,
            matches
        } if requested == "Alpha" && matches == "ALPHA, alpha"
    ));
}

#[test]
fn cli_provider_case_insensitively_selects_one_canonical_custom_id() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "routeryo-copy": {
      "api": "openai-completions",
      "baseUrl": "https://routeryo-copy.example.com",
      "apiKey": "copy-key",
      "models": [{"id":"copy-model"}]
    }
  }
}"#,
    )
    .unwrap();

    let resolved = resolve(Some("ROUTERYO-COPY"), Some("copy-model"), None, None, None).unwrap();
    assert_eq!(resolved.model.provider, "routeryo-copy");
    assert_eq!(resolved.provider.id(), "routeryo-copy");
    assert_eq!(resolved.model.base_url, "https://routeryo-copy.example.com");

    let prefixed = resolve(None, Some("ROUTERYO-COPY/copy-model"), None, None, None).unwrap();
    assert_eq!(prefixed.model.provider, "routeryo-copy");
    assert_eq!(prefixed.model.id, "copy-model");
}

#[test]
fn cli_provider_canonicalization_keeps_builtin_and_custom_case_ids_distinct() {
    let _env = TestEnv::new();
    std::env::set_var(OPENAI_API_KEY_ENV, "official-openai-key");
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "OpenAI": {
      "api": "openai-completions",
      "baseUrl": "https://custom-openai.example.com",
      "apiKey": "custom-openai-key",
      "models": [{"id":"custom-model"}]
    }
  }
}"#,
    )
    .unwrap();

    let custom = resolve(Some("OpenAI"), Some("custom-model"), None, None, None).unwrap();
    assert_eq!(custom.model.provider, "OpenAI");
    assert_eq!(custom.model.base_url, "https://custom-openai.example.com");

    let builtin = resolve(Some("openai"), Some("gpt-6-astra"), None, None, None).unwrap();
    assert_eq!(builtin.model.provider, "openai");

    let error = resolve(Some("OPENAI"), None, None, None, None).unwrap_err();
    assert!(matches!(
        error,
        ResolveError::AmbiguousProvider {
            requested,
            matches
        } if requested == "OPENAI" && matches == "OpenAI, openai"
    ));
}

#[test]
fn cli_api_key_overrides_only_the_selected_anthropic_provider() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "authHeader": true,
      "apiKey": "configured-key",
      "models": [{"id":"gateway-model"}]
    }
  }
}"#,
    )
    .unwrap();

    let cfg = config::load_models_config().unwrap();
    let credentials = resolve_anthropic_credentials(
        &cfg,
        &config::read_auth().unwrap(),
        &[],
        &crate::test_host::TestValues,
    );
    assert_eq!(
        credential_for_selected_anthropic_provider(Some("cli-key"), &credentials, "gateway", true,),
        Some(AnthropicCredential::Headers(BTreeMap::from([(
            "authorization".into(),
            "Bearer cli-key".into(),
        )])))
    );
    assert_eq!(
        anthropic_credential_for(&credentials, "gateway"),
        Some(&AnthropicCredential::Headers(BTreeMap::from([(
            "authorization".into(),
            "Bearer configured-key".into(),
        )])))
    );

    let resolved = resolve(
        Some("gateway"),
        Some("gateway/gateway-model"),
        None,
        Some("cli-key"),
        None,
    )
    .unwrap();
    assert!(!resolved.has_provider_key);
    assert_eq!(
        resolved
            .execution
            .request_model(&resolved.model)
            .headers
            .as_ref()
            .and_then(|headers| headers.get("authorization"))
            .map(String::as_str),
        Some("Bearer cli-key")
    );
}

#[test]
fn auth_token_routes_via_bearer_header() {
    let _env = TestEnv::new();
    std::env::set_var(ANTHROPIC_AUTH_TOKEN_ENV, "tok-123");
    let r = resolve(None, None, None, None, None).unwrap();
    // No provider key carries auth — it lives on the model header.
    let headers = r
        .execution
        .request_model(&r.model)
        .headers
        .expect("bearer header on model");
    assert_eq!(
        headers.get("authorization").map(|s| s.as_str()),
        Some("Bearer tok-123")
    );
    // ANTHROPIC_AUTH_TOKEN is a *global* credential (not endpoint-specific
    // like a models.json gateway key): the default claude-sonnet-5 is picked
    // (it carries the env Bearer) — NOT a gateway model.
    assert_eq!(r.model.id, DEFAULT_MODEL_ID);
}

#[test]
fn api_key_flag_beats_env_and_stored() {
    let _env = TestEnv::new();
    std::env::set_var(ANTHROPIC_API_KEY_ENV, "env-key");
    config::upsert_credential(
        DEFAULT_PROVIDER_ID,
        Credential::ApiKey {
            key: Some("stored-key".into()),
            env: None,
        },
    )
    .unwrap();
    // `--api-key flag-key` wins; resolve succeeds + takes the x-api-key path
    // (no Bearer header on the model).
    let r = resolve(None, None, None, Some("flag-key"), None).unwrap();
    assert!(
        r.execution
            .request_model(&r.model)
            .headers
            .as_ref()
            .and_then(|h| h.get("authorization"))
            .is_none(),
        "--api-key should take the x-api-key path, not Bearer"
    );
}

#[test]
fn base_url_override_applies_to_model() {
    let _env = TestEnv::new();
    std::env::set_var(ANTHROPIC_API_KEY_ENV, "k");
    let r = resolve(None, None, None, None, Some("https://gw.example.com")).unwrap();
    assert_eq!(r.model.base_url, "https://gw.example.com");
}

#[test]
fn base_url_env_is_fallback_for_flag() {
    let _env = TestEnv::new();
    std::env::set_var(ANTHROPIC_API_KEY_ENV, "k");
    std::env::set_var(ANTHROPIC_BASE_URL_ENV, "https://env-gw.example.com");
    let r = resolve(None, None, None, None, None).unwrap();
    assert_eq!(r.model.base_url, "https://env-gw.example.com");
}

#[test]
fn anthropic_base_url_env_does_not_redirect_a_custom_provider() {
    let _env = TestEnv::new();
    std::env::set_var(ANTHROPIC_BASE_URL_ENV, "https://ambient-proxy.example.com");
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "gateway": {
      "api": "anthropic-messages",
      "baseUrl": "https://gateway.example.com",
      "apiKey": "gateway-secret",
      "models": [{"id":"gateway-model"}]
    }
  }
}"#,
    )
    .unwrap();

    let r = resolve(
        Some("gateway"),
        Some("gateway/gateway-model"),
        None,
        None,
        None,
    )
    .unwrap();
    assert_eq!(r.model.base_url, "https://gateway.example.com");
    assert!(r.has_provider_key);
}

#[test]
fn models_json_adds_custom_model() {
    let _env = TestEnv::new();
    std::env::set_var(ANTHROPIC_API_KEY_ENV, "k");
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "authHeader": true,
      "apiKey": "gw-secret",
      "models": [
        { "id": "custom-claude", "name": "Custom" }
      ]
    }
  }
}"#,
    )
    .unwrap();
    let r = resolve(None, Some("custom-claude"), None, None, None).unwrap();
    assert_eq!(r.model.id, "custom-claude");
    assert_eq!(r.model.base_url, "https://gw.example.com");
    assert_eq!(r.model.provider, "gateway");
    assert_eq!(r.provider.id(), "gateway");
    // Provider-level authHeader folded in.
    let headers = r
        .execution
        .request_model(&r.model)
        .headers
        .expect("headers merged");
    assert_eq!(
        headers.get("authorization").map(|s| s.as_str()),
        Some("Bearer gw-secret")
    );
}

#[test]
fn resolved_providers_include_all_authenticated_runtimes() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "gateway-a": {
      "baseUrl": "https://a.example.com",
      "api": "openai-completions",
      "apiKey": "key-a",
      "models": [ { "id": "model-a" } ]
    },
    "gateway-b": {
      "baseUrl": "https://b.example.com",
      "api": "openai-completions",
      "apiKey": "key-b",
      "models": [ { "id": "model-b" } ]
    }
  }
}"#,
    )
    .unwrap();

    let resolved = resolve(None, None, None, None, None).unwrap();
    let ids: Vec<&str> = resolved.providers.iter().map(|p| p.id()).collect();
    // Both runtimes are registered so the harness can hot-switch by
    // `model.provider`; the selected one comes first.
    assert_eq!(resolved.provider.id(), "gateway-a");
    assert_eq!(ids[0], "gateway-a");
    assert!(
        ids.contains(&"gateway-b"),
        "gateway-b runtime missing: {ids:?}"
    );
}

#[test]
fn authenticated_catalog_spans_all_configured_providers() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "gateway-a": {
      "baseUrl": "https://a.example.com",
      "api": "openai-completions",
      "apiKey": "key-a",
      "models": [ { "id": "model-a" } ]
    },
    "gateway-b": {
      "baseUrl": "https://b.example.com",
      "api": "openai-completions",
      "apiKey": "key-b",
      "models": [ { "id": "model-b" } ]
    }
  }
}"#,
    )
    .unwrap();

    let resolved = resolve(None, None, None, None, None).unwrap();
    let catalog = authenticated_catalog(&resolved);
    let providers: std::collections::BTreeSet<&str> =
        catalog.iter().map(|m| m.provider.as_str()).collect();
    assert!(
        providers.contains("gateway-a"),
        "gateway-a models missing: {providers:?}"
    );
    assert!(
        providers.contains("gateway-b"),
        "gateway-b models missing: {providers:?}"
    );
    // Unauthenticated built-ins are excluded (no ANTHROPIC/OPENAI key here).
    assert!(!providers.contains("anthropic"));
    assert!(!providers.contains("openai"));
    // Auth headers merged at build time survive into the selector catalog.
    assert!(catalog.iter().all(|m| m.headers.is_none()));
}

#[test]
fn openai_completions_models_json_is_a_complete_provider_config() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "routeryo": {
      "baseUrl": "https://api.routeryo.com",
      "api": "openai-completions",
      "apiKey": "router-secret",
      "models": [
        {
          "id": "gpt-5.6-sol",
          "name": "GPT 5.6",
          "reasoning": true,
          "contextWindow": 200000,
          "maxTokens": 32768
        }
      ]
    }
  }
}"#,
    )
    .unwrap();

    let resolved = resolve(None, None, None, None, None).unwrap();
    assert_eq!(resolved.model.id, "gpt-5.6-sol");
    assert_eq!(resolved.model.api, rpi_ai::Api::OpenaiCompletions);
    assert_eq!(resolved.model.provider, "routeryo");
    assert_eq!(resolved.provider.id(), "routeryo");
    assert!(resolved.has_provider_key);
    assert_eq!(
        resolved
            .execution
            .request_model(&resolved.model)
            .headers
            .as_ref()
            .and_then(|headers| headers.get("authorization"))
            .map(String::as_str),
        Some("Bearer router-secret")
    );

    let explicit = resolve(
        Some("routeryo"),
        Some("routeryo/gpt-5.6-sol"),
        None,
        None,
        None,
    )
    .unwrap();
    assert_eq!(explicit.provider.id(), "routeryo");
    assert_eq!(explicit.model.id, "gpt-5.6-sol");
}

#[test]
fn openai_completions_provider_alias_excludes_responses_models() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "chat-gateway": {
      "api": "openai-completions",
      "baseUrl": "https://chat.example.com/v1",
      "apiKey": "chat-key",
      "models": [{"id":"chat-model"}]
    },
    "responses-gateway": {
      "api": "openai-responses",
      "baseUrl": "https://responses.example.com/v1",
      "apiKey": "responses-key",
      "models": [{"id":"responses-model"}]
    }
  }
}"#,
    )
    .unwrap();

    let resolved = resolve(Some("openai-completions"), None, None, None, None).unwrap();
    assert_eq!(resolved.model.api, rpi_ai::Api::OpenaiCompletions);
    assert_eq!(resolved.model.provider, "chat-gateway");
    assert_eq!(resolved.model.id, "chat-model");
}

#[test]
fn openai_model_prefix_disambiguates_providers_with_the_same_model_id() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "baseUrl": "https://alpha.example.com",
      "apiKey": "alpha-secret",
      "models": [{"id":"shared-model"}]
    },
    "beta": {
      "api": "openai-completions",
      "baseUrl": "https://beta.example.com",
      "apiKey": "beta-secret",
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
    )
    .unwrap();

    let alpha = resolve(None, Some("alpha/shared-model"), None, None, None).unwrap();
    assert_eq!(alpha.provider.id(), "alpha");
    assert_eq!(alpha.model.base_url, "https://alpha.example.com");
    assert_eq!(
        alpha
            .execution
            .request_model(&alpha.model)
            .headers
            .as_ref()
            .and_then(|headers| headers.get("authorization"))
            .map(String::as_str),
        Some("Bearer alpha-secret")
    );

    let beta = resolve(None, Some("beta/shared-model"), None, None, None).unwrap();
    assert_eq!(beta.provider.id(), "beta");
    assert_eq!(beta.model.base_url, "https://beta.example.com");
    assert_eq!(
        beta.execution
            .request_model(&beta.model)
            .headers
            .as_ref()
            .and_then(|headers| headers.get("authorization"))
            .map(String::as_str),
        Some("Bearer beta-secret")
    );
}

#[test]
fn bare_duplicate_model_id_prefers_the_only_authenticated_provider() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "baseUrl": "https://alpha.example.com",
      "models": [{"id":"shared-model"}]
    },
    "beta": {
      "api": "openai-completions",
      "baseUrl": "https://beta.example.com",
      "apiKey": "beta-secret",
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
    )
    .unwrap();

    let resolved = resolve(None, Some("shared-model"), None, None, None).unwrap();
    assert_eq!(resolved.model.provider, "beta");
    assert_eq!(resolved.model.base_url, "https://beta.example.com");
}

#[test]
fn bare_duplicate_model_id_rejects_multiple_authenticated_providers() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "apiKey": "alpha-secret",
      "models": [{"id":"shared-model"}]
    },
    "beta": {
      "api": "openai-completions",
      "apiKey": "beta-secret",
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
    )
    .unwrap();

    let error = resolve(None, Some("shared-model"), None, None, None).unwrap_err();
    match error {
        ResolveError::AmbiguousModel {
            pattern,
            matches,
            auth_hint,
        } => {
            assert_eq!(pattern, "shared-model");
            assert_eq!(matches, "alpha/shared-model, beta/shared-model");
            assert_eq!(
                auth_hint,
                "More than one matching provider is authenticated."
            );
        }
        other => panic!("expected AmbiguousModel, got {other:?}"),
    }
}

#[test]
fn bare_duplicate_model_id_rejects_when_no_provider_is_authenticated() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "alpha": {
      "api": "openai-completions",
      "models": [{"id":"shared-model"}]
    },
    "beta": {
      "api": "openai-completions",
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
    )
    .unwrap();

    let error = resolve(None, Some("shared-model"), None, None, None).unwrap_err();
    assert!(matches!(
        error,
        ResolveError::AmbiguousModel {
            auth_hint: "No matching provider is authenticated.",
            ..
        }
    ));
}

#[test]
fn openai_auth_json_overrides_config_for_both_protocols() {
    let _env = TestEnv::new();
    config::upsert_credential(
        "chat-gateway",
        Credential::ApiKey {
            key: Some("stored-chat-key".into()),
            env: None,
        },
    )
    .unwrap();
    config::upsert_credential(
        "responses-gateway",
        Credential::ApiKey {
            key: Some("stored-responses-key".into()),
            env: None,
        },
    )
    .unwrap();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "chat-gateway": {
      "api": "openai-completions",
      "baseUrl": "https://chat.example.com/v1",
      "apiKey": "configured-chat-key",
      "models": [{"id":"chat-model"}]
    },
    "responses-gateway": {
      "api": "openai-responses",
      "baseUrl": "https://responses.example.com/v1",
      "apiKey": "configured-responses-key",
      "models": [{"id":"responses-model"}]
    }
  }
}"#,
    )
    .unwrap();

    let chat = resolve(
        Some("chat-gateway"),
        Some("chat-gateway/chat-model"),
        None,
        None,
        None,
    )
    .unwrap();
    assert!(chat.has_provider_key);
    assert_eq!(
        chat.execution
            .request_model(&chat.model)
            .headers
            .as_ref()
            .and_then(|headers| headers.get("authorization"))
            .map(String::as_str),
        Some("Bearer stored-chat-key")
    );

    let responses = resolve(
        Some("responses-gateway"),
        Some("responses-gateway/responses-model"),
        None,
        None,
        None,
    )
    .unwrap();
    assert!(responses.has_provider_key);
    assert_eq!(
        responses
            .execution
            .request_model(&responses.model)
            .headers
            .as_ref()
            .and_then(|headers| headers.get("authorization"))
            .map(String::as_str),
        Some("Bearer stored-responses-key")
    );
}

#[test]
fn openai_env_does_not_authenticate_custom_providers() {
    let _env = TestEnv::new();
    std::env::set_var(OPENAI_API_KEY_ENV, "official-openai-key");
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "chat-gateway": {
      "api": "openai-completions",
      "baseUrl": "https://chat.example.com/v1",
      "models": [{"id":"chat-model"}]
    },
    "responses-gateway": {
      "api": "openai-responses",
      "baseUrl": "https://responses.example.com/v1",
      "models": [{"id":"responses-model"}]
    },
    "openai-completions": {
      "api": "openai-completions",
      "baseUrl": "https://alias-chat.example.com/v1",
      "models": [{"id":"alias-chat-model"}]
    },
    "openai-responses": {
      "api": "openai-responses",
      "baseUrl": "https://alias-responses.example.com/v1",
      "models": [{"id":"alias-responses-model"}]
    },
    "OpenAI": {
      "api": "openai-responses",
      "baseUrl": "https://case-distinct.example.com/v1",
      "models": [{"id":"case-distinct-model"}]
    }
  }
}"#,
    )
    .unwrap();

    for (provider, model) in [
        ("chat-gateway", "chat-gateway/chat-model"),
        ("responses-gateway", "responses-gateway/responses-model"),
        ("openai-completions", "openai-completions/alias-chat-model"),
        ("openai-responses", "openai-responses/alias-responses-model"),
        ("OpenAI", "OpenAI/case-distinct-model"),
    ] {
        let error = resolve(Some(provider), Some(model), None, None, None).unwrap_err();
        assert!(matches!(error, ResolveError::NoApiKey { .. }));
    }

    let official = resolve(None, None, None, None, None).unwrap();
    assert_eq!(official.provider.id(), "openai");
    assert!(official.has_provider_key);
}

#[test]
fn cli_api_key_overrides_selected_openai_provider() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "chat-gateway": {
      "api": "openai-completions",
      "baseUrl": "https://chat.example.com/v1",
      "apiKey": "configured-chat-key",
      "models": [{"id":"chat-model"}]
    },
    "responses-gateway": {
      "api": "openai-responses",
      "baseUrl": "https://responses.example.com/v1",
      "apiKey": "configured-responses-key",
      "models": [{"id":"responses-model"}]
    }
  }
}"#,
    )
    .unwrap();

    for (provider, model) in [
        ("chat-gateway", "chat-gateway/chat-model"),
        ("responses-gateway", "responses-gateway/responses-model"),
    ] {
        let resolved = resolve(Some(provider), Some(model), None, Some("cli-key"), None).unwrap();
        assert_eq!(resolved.provider.id(), provider);
        assert!(resolved.has_provider_key);
        assert_eq!(
            resolved
                .execution
                .request_model(&resolved.model)
                .headers
                .as_ref()
                .and_then(|headers| headers.get("authorization"))
                .map(String::as_str),
            Some("Bearer cli-key")
        );
    }
}

#[test]
fn anthropic_model_prefix_disambiguates_providers_with_the_same_model_id() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "alpha": {
      "api": "anthropic-messages",
      "baseUrl": "https://alpha.example.com",
      "apiKey": "alpha-secret",
      "models": [{"id":"shared-model"}]
    },
    "beta": {
      "api": "anthropic-messages",
      "baseUrl": "https://beta.example.com",
      "authHeader": true,
      "apiKey": "beta-secret",
      "models": [{"id":"shared-model"}]
    }
  }
}"#,
    )
    .unwrap();

    let alpha = resolve(None, Some("alpha/shared-model"), None, None, None).unwrap();
    assert_eq!(alpha.provider.id(), "alpha");
    assert_eq!(alpha.model.provider, "alpha");
    assert_eq!(alpha.model.base_url, "https://alpha.example.com");
    assert!(alpha.has_provider_key);
    assert!(alpha
        .execution
        .request_model(&alpha.model)
        .headers
        .as_ref()
        .map_or(true, |headers| {
            headers
                .keys()
                .all(|name| !name.eq_ignore_ascii_case("x-api-key"))
        }));

    let beta = resolve(None, Some("beta/shared-model"), None, None, None).unwrap();
    assert_eq!(beta.provider.id(), "beta");
    assert_eq!(beta.model.provider, "beta");
    assert_eq!(beta.model.base_url, "https://beta.example.com");
    assert_eq!(
        beta.execution
            .request_model(&beta.model)
            .headers
            .as_ref()
            .and_then(|headers| headers.get("authorization"))
            .map(String::as_str),
        Some("Bearer beta-secret")
    );
}

#[test]
fn unknown_model_prefix_without_a_raw_match_returns_no_match() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "routeryo": {
      "api": "openai-completions",
      "apiKey": "secret",
      "models": [{"id":"gpt-test"}]
    }
  }
}"#,
    )
    .unwrap();

    let error = resolve(None, Some("misspelled/gpt-test"), None, None, None).unwrap_err();
    assert!(matches!(
        error,
        ResolveError::NoMatch { pattern, .. } if pattern == "misspelled/gpt-test"
    ));
}

/// A models.json gateway with `authHeader:true` + `apiKey` is itself an auth
/// source — it satisfies the `resolve` auth gate WITHOUT any env var, stored
/// cred, or `--api-key`. This is the "models.json file alone sets up a
/// third-party endpoint" path. The Bearer folds onto the gateway model only
/// (built-in claude-* stays Bearer-less), and — with no `--model` — the
/// default selector picks that gateway model (the only authed one).
#[test]
fn models_json_auth_header_satisfies_auth_without_env() {
    let _env = TestEnv::new();
    // No ANTHROPIC_* env, no auth.json — only the models.json gateway.
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "api": "anthropic-messages",
      "authHeader": true,
      "apiKey": "gw-secret",
      "models": [
        { "id": "custom-claude", "contextWindow": 200000, "maxTokens": 8192 }
      ]
    }
  }
}"#,
    )
    .unwrap();
    let r = resolve(None, Some("custom-claude"), None, None, None).unwrap();
    assert_eq!(r.model.id, "custom-claude");
    assert_eq!(r.model.base_url, "https://gw.example.com");
    let headers = r
        .execution
        .request_model(&r.model)
        .headers
        .expect("bearer folded onto model");
    assert_eq!(
        headers.get("authorization").map(|s| s.as_str()),
        Some("Bearer gw-secret")
    );
}

/// The `--api-key` flag wins over a models.json `authHeader:true` gateway
/// key while preserving the provider's Bearer transport contract.
/// A `models.json`-only gateway config (no `--model`, no env, no auth.json)
/// should pick the gateway model by default — mirroring the TS
/// `findInitialModel` step-4 fallback `availableModels[0]` over the
/// auth-filtered snapshot. The built-in Anthropic models carry no auth in a
/// gateway-only setup, so the gateway model is the first (and only)
/// authenticated model. This is the `rpi -p hi` (no `--model`) case.
#[test]
fn default_prefers_gateway_when_only_gateway_configured() {
    // TestEnv already holds the shared env_lock for its whole lifetime —
    // don't take it again here (would self-deadlock and poison the mutex).
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "api": "anthropic-messages",
      "authHeader": true,
      "apiKey": "gw-secret",
      "models": [
        { "id": "custom-claude", "contextWindow": 200000, "maxTokens": 8192 }
      ]
    }
  }
}"#,
    )
    .unwrap();
    // No --model (None): the default selector must pick the gateway model,
    // NOT the built-in claude-sonnet-5 (which would carry a foreign Bearer
    // to api.anthropic.com → 401, the bug this fixes).
    let r = resolve(None, None, None, None, None).unwrap();
    assert_eq!(r.model.id, "custom-claude");
    assert_eq!(r.model.base_url, "https://gw.example.com");
    // Gateway model carries the folded Bearer.
    let headers = r
        .execution
        .request_model(&r.model)
        .headers
        .expect("bearer on gateway model");
    assert_eq!(
        headers.get("authorization").map(|s| s.as_str()),
        Some("Bearer gw-secret")
    );
}

#[test]
fn api_key_flag_honors_models_json_auth_header() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "authHeader": true,
      "apiKey": "gw-secret",
      "models": [ { "id": "custom-claude" } ]
    }
  }
}"#,
    )
    .unwrap();
    let r = resolve(None, Some("custom-claude"), None, Some("flag-key"), None).unwrap();
    assert_eq!(
        r.execution
            .request_model(&r.model)
            .headers
            .as_ref()
            .and_then(|h| h.get("authorization"))
            .map(String::as_str),
        Some("Bearer flag-key")
    );
    assert!(!r.has_provider_key);
}

/// A models.json gateway with a **bare** `apiKey` (no `authHeader`) is the
/// `composeApiKeyAuth` arm — it satisfies the `resolve` auth gate WITHOUT
/// any env var, stored cred, or `--api-key`, routing the resolved key as
/// the selected provider's default `x-api-key`. It is not copied into model
/// headers, so a different provider can never inherit it. This is the
/// default copied-pi `models.json` shape.
#[test]
fn models_json_bare_apikey_satisfies_auth_without_env() {
    let _env = TestEnv::new();
    // No ANTHROPIC_* env, no auth.json — only the bare-apiKey models.json gateway.
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "api": "anthropic-messages",
      "apiKey": "gw-secret",
      "models": [
        { "id": "custom-claude", "contextWindow": 200000, "maxTokens": 8192 }
      ]
    }
  }
}"#,
    )
    .unwrap();
    let r = resolve(None, Some("custom-claude"), None, None, None).unwrap();
    assert_eq!(r.model.id, "custom-claude");
    assert_eq!(r.model.base_url, "https://gw.example.com");
    assert!(r.has_provider_key);
    assert!(r
        .execution
        .request_model(&r.model)
        .headers
        .as_ref()
        .map_or(true, |headers| {
            headers.keys().all(|name| {
                !name.eq_ignore_ascii_case("x-api-key")
                    && !name.eq_ignore_ascii_case("authorization")
            })
        }));
}

/// The bare-`apiKey` x-api-key fold is endpoint-specific: with no `--model`,
/// the default selector must pick the gateway model (the only authed one),
// NOT the built-in claude-sonnet-5 — which would carry a gateway x-api-key to
// api.anthropic.com → 401, the same misrouting the Bearer fold guards
// against. This is the `rpi -p hi` (no `--model`) case for a bare-apiKey
/// gateway.
#[test]
fn default_prefers_gateway_when_only_bare_apikey_configured() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "api": "anthropic-messages",
      "apiKey": "gw-secret",
      "models": [
        { "id": "custom-claude", "contextWindow": 200000, "maxTokens": 8192 }
      ]
    }
  }
}"#,
    )
    .unwrap();
    // No --model (None): the default selector must pick the gateway model.
    let r = resolve(None, None, None, None, None).unwrap();
    assert_eq!(r.model.id, "custom-claude");
    assert_eq!(r.model.base_url, "https://gw.example.com");
    assert!(r.has_provider_key);
}

/// A bare `apiKey` that references an unset env var resolves to `None` and
/// is skipped (mirrors pi `resolveConfigValue` semantics) — the auth gate
/// falls through to the env/`rpi auth login` sources rather than partially
/// authenticating with an empty key.
#[test]
fn models_json_bare_apikey_env_template_resolves() {
    let _env = TestEnv::new();
    // Prime the env var the apiKey references.
    std::env::set_var("RPI_TEST_GATEWAY_KEY", "env-resolved-secret");
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "api": "anthropic-messages",
      "apiKey": "$RPI_TEST_GATEWAY_KEY",
      "models": [
        { "id": "custom-claude", "contextWindow": 200000, "maxTokens": 8192 }
      ]
    }
  }
}"#,
    )
    .unwrap();
    let r = resolve(None, Some("custom-claude"), None, None, None).unwrap();
    assert!(r.has_provider_key);
    let cfg = config::load_models_config().unwrap();
    let credentials = resolve_anthropic_credentials(
        &cfg,
        &config::read_auth().unwrap(),
        &[],
        &crate::test_host::TestValues,
    );
    assert_eq!(
        anthropic_credential_for(&credentials, "gateway"),
        Some(&AnthropicCredential::ProviderKey(
            "env-resolved-secret".into()
        ))
    );
    std::env::remove_var("RPI_TEST_GATEWAY_KEY");
}

/// `authHeader: true` takes precedence over a bare `apiKey` on the SAME or a
/// later provider: the Bearer step (3a) runs before the bare-apiKey step
/// A models.json with BOTH auth shapes — `authHeader:true` and bare
/// `apiKey` — routes each provider's credential onto ITS OWN models
/// (per-provider fold, mirroring upstream `composeApiKeyAuth`): the
/// authHeader provider's key becomes `Authorization: Bearer` on its model,
/// the bare-apiKey provider's key becomes `x-api-key` on its model. A
/// copied pi models.json mixing both shapes works end-to-end — no model
/// ends up unauthenticated because another provider "won" the gate.
#[test]
fn auth_header_provider_and_bare_apikey_provider_each_fold_their_own() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "bearer-gw": {
      "baseUrl": "https://bearer.example.com",
      "api": "anthropic-messages",
      "authHeader": true,
      "apiKey": "bearer-secret",
      "models": [ { "id": "bearer-model" } ]
    },
    "xkey-gw": {
      "baseUrl": "https://xkey.example.com",
      "api": "anthropic-messages",
      "apiKey": "xkey-secret",
      "models": [ { "id": "xkey-model" } ]
    }
  }
}"#,
    )
    .unwrap();
    // Both providers satisfy the auth gate together (no env / stored cred
    // needed); the default selector picks the first authed model.
    let r = resolve(None, None, None, None, None).unwrap();
    assert_eq!(r.model.id, "bearer-model");

    // bearer-gw's key folds as Bearer onto bearer-model only.
    let r = resolve(None, Some("bearer-model"), None, None, None).unwrap();
    let h = r
        .execution
        .request_model(&r.model)
        .headers
        .expect("bearer folded");
    assert_eq!(
        h.get("authorization").map(|s| s.as_str()),
        Some("Bearer bearer-secret")
    );
    assert!(
        !h.contains_key("x-api-key"),
        "authHeader path must not synthesize x-api-key"
    );

    // xkey-gw's bare apiKey becomes only that provider's default key.
    let r2 = resolve(None, Some("xkey-model"), None, None, None).unwrap();
    assert!(r2.has_provider_key);
    assert!(r2
        .execution
        .request_model(&r2.model)
        .headers
        .as_ref()
        .map_or(true, |headers| {
            headers.keys().all(|name| {
                !name.eq_ignore_ascii_case("x-api-key")
                    && !name.eq_ignore_ascii_case("authorization")
            })
        }));

    // The active provider exposes only its own catalog. Selecting the
    // other provider explicitly creates a separately keyed provider.
    let catalog = available_catalog(&r);
    let ids: Vec<&str> = catalog.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, vec!["bearer-model"]);
    let other_catalog = available_catalog(&r2);
    let other_ids: Vec<&str> = other_catalog.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(other_ids, vec!["xkey-model"]);
}

/// A copied pi settings.json whose `defaultProvider` names a **models.json
/// gateway** (not "anthropic") must still honor the saved `defaultModel` —
/// pi's `findInitialModel` step-3 applies `defaultModelPerProvider`
/// regardless of provider id. Without this, enabling a second gateway
/// flips the no-`--model` default to the FIRST authed model in catalog
/// order, not the user's saved choice.
#[test]
fn settings_default_model_honored_for_models_json_provider() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "beta-gw": {
      "baseUrl": "https://beta.example.com",
      "api": "anthropic-messages",
      "apiKey": "beta-secret",
      "models": [ { "id": "beta-model" } ]
    },
    "alpha-gw": {
      "baseUrl": "https://alpha.example.com",
      "api": "anthropic-messages",
      "apiKey": "alpha-secret",
      "models": [ { "id": "alpha-model" } ]
    }
  }
}"#,
    )
    .unwrap();
    // Saved default points at the ALPHA gateway's model even though
    // "beta-gw" is declared first and would win first-authed without the
    // settings arm, matching upstream's Object.entries order.
    std::fs::write(
        config::settings_path().unwrap(),
        r#"{"defaultProvider":"alpha-gw","defaultModel":"alpha-model"}"#,
    )
    .unwrap();
    let r = resolve(None, None, None, None, None).unwrap();
    assert_eq!(r.model.id, "alpha-model");
    // An unknown provider id falls through to first-authed (beta-gw).
    std::fs::write(
        config::settings_path().unwrap(),
        r#"{"defaultProvider":"not-a-provider","defaultModel":"beta-model"}"#,
    )
    .unwrap();
    let r = resolve(None, None, None, None, None).unwrap();
    assert_eq!(r.model.id, "beta-model");
}

#[test]
fn models_json_fallback_preserves_provider_and_model_declaration_order() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "routeryo-copy": {
      "api": "openai-completions",
      "baseUrl": "https://router.example.com/v1",
      "apiKey": "router-key",
      "models": [
        { "id": "gpt-5.6-sol" },
        { "id": "gpt-5.6-terra" }
      ]
    },
    "alpha-gw": {
      "api": "openai-completions",
      "baseUrl": "https://alpha.example.com/v1",
      "apiKey": "alpha-key",
      "models": [ { "id": "alpha-model" } ]
    }
  }
}"#,
    )
    .unwrap();

    let resolved = resolve(None, None, None, None, None).unwrap();
    assert_eq!(resolved.model.provider, "routeryo-copy");
    assert_eq!(resolved.model.id, "gpt-5.6-sol");
}

#[test]
fn external_provider_uses_its_own_catalog_order_without_a_builtin_default() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "deepseek": {
      "api": "openai-completions",
      "baseUrl": "https://api.deepseek.com",
      "apiKey": "deepseek-key",
      "models": [
        { "id": "deepseek-chat" },
        { "id": "deepseek-v4-pro" }
      ]
    }
  }
}"#,
    )
    .unwrap();

    let resolved = resolve(None, None, None, None, None).unwrap();
    assert_eq!(resolved.model.provider, "deepseek");
    assert_eq!(resolved.model.id, "deepseek-chat");
}

/// The `/model` selector catalog (`available_catalog`) is auth-filtered —
/// it must NOT offer built-in claude-* models that carry no auth headers in
/// a gateway-only setup (selecting one would fail at request time with
/// "No API key for provider: anthropic"). Mirrors pi's
/// `getAvailableSnapshot` filter (`available = all.filter(m =>
/// configuredProviders.has(m.provider))`): only the gateway model is
/// loadable, so only it appears in the selector / Ctrl+P cycle.
#[test]
fn available_catalog_filters_to_authed_models_in_gateway_only_setup() {
    let _env = TestEnv::new();
    std::fs::write(
        config::models_path().unwrap(),
        r#"{
  "providers": {
    "gateway": {
      "baseUrl": "https://gw.example.com",
      "api": "anthropic-messages",
      "apiKey": "gw-secret",
      "models": [
        { "id": "custom-claude", "contextWindow": 200000, "maxTokens": 8192 }
      ]
    }
  }
}"#,
    )
    .unwrap();
    let r = resolve(None, None, None, None, None).unwrap();
    // A bare models.json key is carried by this provider, not model headers.
    assert!(r.has_provider_key);
    let catalog = available_catalog(&r);
    // Exactly one loadable model: the gateway one. The 7 built-in Anthropic
    // models are filtered out.
    let ids: Vec<&str> = catalog.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["custom-claude"],
        "selector must only list authed models"
    );
    // The runtime provider and selector share the same provider-isolated
    // catalog, so models from another identity cannot be routed through it.
    assert_eq!(r.provider.models().len(), catalog.len());
}

/// On the x-api-key path (`--api-key`/auth.json/`ANTHROPIC_API_KEY`), the
/// provider's default key attaches to EVERY model out-of-band — so the
/// catalog filter keeps the full list (all models are loadable).
#[test]
fn available_catalog_keeps_all_models_on_provider_key_path() {
    let _env = TestEnv::new();
    std::env::set_var(ANTHROPIC_API_KEY_ENV, "k");
    let r = resolve(None, None, None, None, None).unwrap();
    assert!(r.has_provider_key);
    let catalog = available_catalog(&r);
    assert_eq!(catalog.len(), r.provider.models().len());
    assert!(catalog.iter().any(|m| m.id == DEFAULT_MODEL_ID));
}

#[test]
fn case_distinct_wire_model_ids_select_the_exact_route() {
    let mut registry = RuntimeFactories::default();
    registry.declare(crate::ProviderDeclaration {
        id: "case-models".into(),
        models: vec![
            Model::new(
                "Case-ID",
                "Upper",
                rpi_ai::Api::OpenaiCompletions,
                "case-models",
                "https://upper.example/v1",
            ),
            Model::new(
                "case-id",
                "Lower",
                rpi_ai::Api::OpenaiCompletions,
                "case-models",
                "https://lower.example/v1",
            ),
        ],
        api_key_env: None,
        requires_auth: true,
    });
    let mut config = crate::RuntimeConfig::default();
    config.credentials.insert(
        "case-models".into(),
        Credential::ApiKey {
            key: Some("fixture-key".into()),
            env: None,
        },
    );
    for (id, endpoint) in [
        ("Case-ID", "https://upper.example/v1"),
        ("case-id", "https://lower.example/v1"),
    ] {
        let resolved = config
            .resolve(
                &crate::ModelRequest {
                    provider: Some("case-models"),
                    model: Some(id),
                    ..Default::default()
                },
                &registry,
                &BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(resolved.model.id, id);
        assert_eq!(resolved.model.base_url, endpoint);
    }
    let error = config
        .resolve(
            &crate::ModelRequest {
                provider: Some("case-models"),
                model: Some("CASE-ID"),
                ..Default::default()
            },
            &registry,
            &BTreeMap::new(),
        )
        .unwrap_err();
    assert!(matches!(error, ResolveError::AmbiguousModel { .. }));
}

#[test]
fn host_declaration_replaces_builtin_antigravity_route() {
    use rpi_ai::providers::antigravity;

    let models = antigravity::models();
    let source: antigravity::AccountSource = Arc::new(|_| {
        Box::pin(async { Err("Unexpected account lookup during model resolution".into()) })
    });
    let provider = antigravity::AntigravityProvider::with_models(
        reqwest::Client::new(),
        source,
        models.clone(),
    );
    let mut registry = RuntimeFactories::default();
    registry.declare(crate::ProviderDeclaration {
        id: antigravity::ID.into(),
        models,
        api_key_env: None,
        requires_auth: false,
    });
    registry.register(Arc::new(provider), rpi_ai::Api::GoogleGenerativeAi);
    let resolved = crate::RuntimeConfig::default()
        .resolve(
            &crate::ModelRequest {
                provider: Some(antigravity::ID),
                model: Some("gemini-3.8-flash"),
                ..Default::default()
            },
            &registry,
            &BTreeMap::new(),
        )
        .unwrap();

    assert_eq!(resolved.model.api, rpi_ai::Api::GoogleGenerativeAi);
    assert_eq!(resolved.model.base_url, antigravity::ENDPOINT);
    assert_eq!(
        resolved
            .provider
            .models()
            .iter()
            .filter(|model| model.id == "gemini-3.8-flash")
            .count(),
        1,
        "the native host route must replace, not duplicate, the bundled route",
    );
}

#[test]
fn host_declaration_preserves_its_explicit_wire_variants() {
    let mut registry = RuntimeFactories::default();
    registry.declare(crate::ProviderDeclaration {
        id: "openai".into(),
        models: vec![
            Model::new(
                "gpt-6-astra",
                "Chat",
                rpi_ai::Api::OpenaiCompletions,
                "openai",
                "https://chat.example/v1",
            ),
            Model::new(
                "gpt-6-astra",
                "Responses",
                rpi_ai::Api::OpenaiResponses,
                "openai",
                "https://responses.example/v1",
            ),
        ],
        api_key_env: None,
        requires_auth: true,
    });
    let mut config = crate::RuntimeConfig::default();
    config.credentials.insert(
        "openai".into(),
        Credential::ApiKey {
            key: Some("fixture-key".into()),
            env: None,
        },
    );
    for (api, endpoint) in [
        (rpi_ai::Api::OpenaiCompletions, "https://chat.example/v1"),
        (rpi_ai::Api::OpenaiResponses, "https://responses.example/v1"),
    ] {
        let resolved = config
            .resolve(
                &crate::ModelRequest {
                    provider: Some("openai"),
                    model: Some("gpt-6-astra"),
                    api: Some(api.clone()),
                    ..Default::default()
                },
                &registry,
                &BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(resolved.model.api, api);
        assert_eq!(resolved.model.base_url, endpoint);
    }
}
