use rpi_ai::{Api, ThinkingLevel};
use rpi_runtime::{
    credentials::{AuthStatus, CredentialSource},
    ModelRequest, ProviderDeclaration, ProviderRegistry, Runtime, RuntimeConfig, ValueSource,
};
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

#[derive(Default)]
struct CountingValues {
    commands: Mutex<Vec<String>>,
}
impl ValueSource for CountingValues {
    fn env(&self, _: &str) -> Option<String> {
        None
    }
    fn command(&self, command: &str, _: bool) -> Option<String> {
        self.commands.lock().unwrap().push(command.into());
        Some("Bearer fixture-header".into())
    }
}

fn config(value: serde_json::Value) -> RuntimeConfig {
    RuntimeConfig {
        models: serde_json::from_value(value).unwrap(),
        ..Default::default()
    }
}
fn request() -> ModelRequest<'static> {
    ModelRequest {
        provider: Some("account"),
        model: Some("model"),
        ..Default::default()
    }
}

#[test]
fn auth_queries_only_expand_headers_for_the_provider_that_needs_them() {
    let config = config(serde_json::json!({"providers":{
        "account":{"api":"openai-completions", "apiKey":"configured-key", "headers":{"Authorization":"!unused-account"}, "models":[{"id":"model"}]},
        "header-account":{"api":"openai-completions", "headers":{"Authorization":"!needed-header"}, "models":[{"id":"model"}]}
    }}));
    let values = CountingValues::default();
    let providers = ProviderRegistry::default();
    assert!(config
        .auth_status("account", &providers, &values)
        .is_ready());
    assert!(!config
        .auth_status("unknown", &providers, &values)
        .is_ready());
    assert!(values.commands.lock().unwrap().is_empty());
    let states = config.auth_states(&providers, &values);
    assert_eq!(states["header-account"].source, CredentialSource::Headers);
    assert_eq!(*values.commands.lock().unwrap(), ["needed-header"]);
}

#[test]
fn snapshot_reflects_the_active_key_and_endpoint_override_only_for_selected_provider() {
    let config = config(serde_json::json!({"providers":{
        "account":{"api":"openai-completions", "models":[{"id":"model"}]},
        "other":{"api":"openai-completions", "models":[{"id":"model"}]}
    }}));
    let providers = ProviderRegistry::default();
    let values = BTreeMap::<String, String>::new();
    let mut selection = request();
    selection.api_key = Some("private-explicit-key");
    selection.base_url = Some("https://override.example/v1");
    let mut runtime = Runtime::new(&config, &providers, &values, &selection).unwrap();
    let snapshot = runtime.snapshot();
    assert!(snapshot.auth["account"].is_ready());
    assert_eq!(snapshot.auth["other"].status, AuthStatus::Missing);
    let selected = snapshot
        .models
        .iter()
        .find(|model| model.provider == "account")
        .unwrap();
    assert_eq!(selected.base_url, runtime.current().model.base_url);
    assert!(!serde_json::to_string(&snapshot)
        .unwrap()
        .contains("private-explicit-key"));
    let changes = runtime.subscribe();
    selection.api_key = Some("");
    assert!(runtime
        .reload(&config, &providers, &values, &selection)
        .is_err());
    assert!(Arc::ptr_eq(&snapshot, &runtime.snapshot()));
    assert!(!changes.has_changed().unwrap());
}

struct RotatingEnvironment(AtomicUsize);
impl ValueSource for RotatingEnvironment {
    fn env(&self, name: &str) -> Option<String> {
        if name != "ACCOUNT_KEY" {
            return None;
        }
        (self.0.fetch_add(1, Ordering::SeqCst) == 0).then(|| "one-read-key".into())
    }
}

struct RotatingHeader(AtomicUsize);
impl ValueSource for RotatingHeader {
    fn env(&self, _: &str) -> Option<String> {
        None
    }
    fn command(&self, _: &str, cached: bool) -> Option<String> {
        assert!(!cached, "header commands keep their uncached semantics");
        (self.0.fetch_add(1, Ordering::SeqCst) == 0).then(|| "Bearer first-header".into())
    }
}

#[test]
fn snapshots_do_not_rerun_header_commands_and_failed_rotation_keeps_the_binding() {
    let config = config(serde_json::json!({"providers":{
        "account":{"api":"openai-completions", "headers":{"Authorization":"!rotate-header"}, "models":[{"id":"model"}]}
    }}));
    let values = RotatingHeader(AtomicUsize::new(0));
    let providers = ProviderRegistry::default();
    let mut runtime = Runtime::new(&config, &providers, &values, &request()).unwrap();
    assert_eq!(values.0.load(Ordering::SeqCst), 1);
    let current = runtime.current();
    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.auth["account"].source, CredentialSource::Headers);
    let changes = runtime.subscribe();
    assert!(runtime
        .reload(&config, &providers, &values, &request())
        .is_err());
    assert!(Arc::ptr_eq(&current, &runtime.current()));
    assert!(Arc::ptr_eq(&snapshot, &runtime.snapshot()));
    assert!(!changes.has_changed().unwrap());
    assert!(!serde_json::to_string(&snapshot)
        .unwrap()
        .contains("first-header"));
}

#[test]
fn snapshot_reuses_the_credentials_that_were_bound_instead_of_resolving_again() {
    let config = config(serde_json::json!({"providers":{
        "account":{"api":"openai-completions", "models":[{"id":"model"}]}
    }}));
    let mut providers = ProviderRegistry::default();
    providers.declare(ProviderDeclaration {
        id: "account".into(),
        models: vec![],
        api_key_env: Some("ACCOUNT_KEY".into()),
        requires_auth: true,
    });
    let values = RotatingEnvironment(AtomicUsize::new(0));
    let runtime = Runtime::new(&config, &providers, &values, &request()).unwrap();
    assert!(runtime.snapshot().auth["account"].is_ready());
    assert_eq!(values.0.load(Ordering::SeqCst), 1);
}

#[test]
fn non_reasoning_models_always_report_thinking_off_without_losing_requested_level() {
    let config = config(serde_json::json!({"providers":{
        "account":{"api":"openai-completions", "apiKey":"fixture", "models":[{"id":"model", "reasoning":false}]}
    }}));
    for thinking in [
        ThinkingLevel::Off,
        ThinkingLevel::Medium,
        ThinkingLevel::Max,
    ] {
        let mut selection = request();
        selection.thinking = Some(thinking);
        let result = config
            .resolve(
                &selection,
                &ProviderRegistry::default(),
                &BTreeMap::<String, String>::new(),
            )
            .unwrap();
        assert_eq!(result.selection().requested_thinking, thinking);
        assert_eq!(result.selection().effective_thinking, ThinkingLevel::Off);
    }
}

#[test]
fn reasoning_models_clamp_to_their_declared_supported_levels() {
    let config = config(serde_json::json!({"providers":{
        "account":{"api":"openai-completions", "apiKey":"fixture", "models":[{"id":"model", "reasoning":true,"thinkingLevelMap":{"max":null,"xhigh":null,"high":null}}]}
    }}));
    let mut selection = request();
    selection.api = Some(Api::OpenaiCompletions);
    selection.thinking = Some(ThinkingLevel::Max);
    let result = config
        .resolve(
            &selection,
            &ProviderRegistry::default(),
            &BTreeMap::<String, String>::new(),
        )
        .unwrap();
    assert_eq!(result.selection().effective_thinking, ThinkingLevel::Medium);
}
