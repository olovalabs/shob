use rpi_ai::{Api, AssistantMessageEventStream, Context, Model, Provider, SimpleStreamOptions};
use rpi_runtime::{
    config::{AuthStore, Credential},
    credentials::{AuthStatus, CredentialSource},
    ModelRequest, ProviderDeclaration, ProviderRegistry, ResolveError, Runtime, RuntimeConfig,
    ValueSource,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Calls(Mutex<Vec<(Model, SimpleStreamOptions)>>);
struct HostProvider {
    models: Vec<Model>,
    calls: Arc<Calls>,
}
#[async_trait::async_trait]
impl Provider for HostProvider {
    fn id(&self) -> &str {
        "account"
    }
    fn models(&self) -> &[Model] {
        &self.models
    }
    async fn stream_simple(
        &self,
        model: &Model,
        _: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        self.calls
            .0
            .lock()
            .unwrap()
            .push((model.clone(), options.clone()));
        let (mut producer, stream) = rpi_ai::event_stream::create_assistant_message_event_stream();
        producer
            .push(rpi_ai::AssistantMessageEvent::Done {
                reason: rpi_ai::DoneReason::Stop,
                message: rpi_ai::AssistantMessage::empty(
                    model.api.clone(),
                    model.provider.clone(),
                    model.id.clone(),
                    0,
                ),
            })
            .await;
        stream
    }
}
fn registry(calls: Arc<Calls>, api: Api) -> ProviderRegistry {
    let model = Model::new(
        "same",
        "Same",
        api.clone(),
        "account",
        "https://host.example/v1",
    );
    let mut providers = ProviderRegistry::default();
    providers.declare(ProviderDeclaration {
        id: "account".into(),
        models: vec![model.clone()],
        api_key_env: Some("ACCOUNT_KEY".into()),
        requires_auth: true,
    });
    providers.register(
        Arc::new(HostProvider {
            models: vec![model],
            calls,
        }),
        api,
    );
    providers
}
fn request() -> ModelRequest<'static> {
    ModelRequest {
        provider: Some("account"),
        model: Some("same"),
        ..Default::default()
    }
}

#[tokio::test]
async fn hosts_are_isolated_and_old_bindings_survive_failed_and_successful_reload() {
    let calls = Arc::new(Calls::default());
    let providers = registry(calls.clone(), Api::OpenaiCompletions);
    let config = RuntimeConfig::default();
    let first = BTreeMap::from([("ACCOUNT_KEY".into(), "first-secret".into())]);
    let second = BTreeMap::from([("ACCOUNT_KEY".into(), "second-secret".into())]);
    let mut runtime = Runtime::new(&config, &providers, &first, &request()).unwrap();
    let old = runtime.current();
    let mut changes = runtime.subscribe();
    let snapshot = runtime.snapshot();
    assert!(runtime
        .reload(
            &config,
            &providers,
            &BTreeMap::<String, String>::new(),
            &request()
        )
        .is_err());
    assert!(Arc::ptr_eq(&snapshot, &runtime.snapshot()));
    assert!(!changes.has_changed().unwrap());
    runtime
        .reload(&config, &providers, &second, &request())
        .unwrap();
    assert!(changes.has_changed().unwrap());
    assert_eq!(changes.borrow_and_update().revision, 1);
    for active in [old, runtime.current()] {
        assert!(active.model.headers.is_none());
        active
            .provider
            .stream_simple(
                &active.model,
                &Context::new(vec![]),
                &SimpleStreamOptions::default(),
            )
            .await
            .result()
            .await
            .unwrap();
    }
    let calls = calls.0.lock().unwrap();
    assert_eq!(calls[0].1.api_key.as_deref(), Some("first-secret"));
    assert_eq!(calls[1].1.api_key.as_deref(), Some("second-secret"));
    let serialized = serde_json::to_string(&*runtime.snapshot()).unwrap();
    assert!(!serialized.contains("first-secret") && !serialized.contains("second-secret"));
}

struct FixedTime;
impl ValueSource for FixedTime {
    fn env(&self, _: &str) -> Option<String> {
        None
    }
    fn now_unix_seconds(&self) -> i64 {
        100
    }
}
#[test]
fn readiness_distinguishes_expiry_fallback_headers_anonymous_and_missing() {
    let mut config = RuntimeConfig::default();
    let mut providers = registry(Arc::default(), Api::OpenaiCompletions);
    assert_eq!(
        config.auth_status("account", &providers, &FixedTime).status,
        AuthStatus::Missing
    );
    config.credentials.insert(
        "account".into(),
        Credential::Oauth {
            access: "expired-secret".into(),
            refresh: "private-refresh".into(),
            expires: 99,
        },
    );
    assert_eq!(
        config.auth_status("account", &providers, &FixedTime).status,
        AuthStatus::Expired
    );
    config.models = serde_json::from_value(serde_json::json!({"providers":{"account":{"api":"openai-completions","apiKey":"replacement-secret"}}})).unwrap();
    let state = config.auth_status("account", &providers, &FixedTime);
    assert_eq!(state.status, AuthStatus::Ready);
    assert_eq!(state.source, CredentialSource::ModelsJson);
    config.models.providers.get_mut("account").unwrap().api_key = None;
    config.models.providers.get_mut("account").unwrap().headers = Some(BTreeMap::from([(
        "Authorization".into(),
        "Bearer header-secret".into(),
    )]));
    assert_eq!(
        config.auth_status("account", &providers, &FixedTime).source,
        CredentialSource::Headers
    );
    config.models = Default::default();
    providers.declare(ProviderDeclaration {
        id: "account".into(),
        models: vec![],
        api_key_env: None,
        requires_auth: false,
    });
    assert_eq!(
        config.auth_status("account", &providers, &FixedTime).status,
        AuthStatus::Anonymous
    );
    assert!(!serde_json::to_string(&state).unwrap().contains("secret"));
}

#[test]
fn model_request_can_disambiguate_one_provider_by_wire_api() {
    let mut providers = registry(Arc::default(), Api::OpenaiCompletions);
    let responses = Model::new(
        "same",
        "Responses",
        Api::OpenaiResponses,
        "account",
        "https://responses.example/v1",
    );
    let completions = Model::new(
        "same",
        "Completions",
        Api::OpenaiCompletions,
        "account",
        "https://chat.example/v1",
    );
    providers.declare(ProviderDeclaration {
        id: "account".into(),
        models: vec![completions, responses],
        api_key_env: None,
        requires_auth: false,
    });
    let mut selection = request();
    selection.api = Some(Api::OpenaiResponses);
    selection.thinking = Some(rpi_ai::ThinkingLevel::Max);
    let resolved = RuntimeConfig::default()
        .resolve(&selection, &providers, &FixedTime)
        .unwrap();
    assert_eq!(resolved.model.api, Api::OpenaiResponses);
    assert_eq!(resolved.selection().identity.api, Api::OpenaiResponses);
    assert_eq!(
        resolved.selection().requested_thinking,
        rpi_ai::ThinkingLevel::Max
    );
    assert_eq!(
        resolved.selection().effective_thinking,
        rpi_ai::ThinkingLevel::Off
    );
}

#[test]
fn unsupported_declared_protocol_returns_an_error_without_panicking() {
    let mut providers = ProviderRegistry::default();
    providers.declare(ProviderDeclaration {
        id: "account".into(),
        models: vec![Model::new(
            "same",
            "Same",
            Api::Other("custom-api".into()),
            "account",
            "https://host.example",
        )],
        api_key_env: None,
        requires_auth: false,
    });
    assert!(matches!(
        RuntimeConfig::default().resolve(&request(), &providers, &FixedTime),
        Err(ResolveError::UnsupportedApi { .. })
    ));
}

#[tokio::test]
async fn two_registered_implementations_for_one_provider_keep_their_api_routes() {
    let completions = Arc::new(Calls::default());
    let responses = Arc::new(Calls::default());
    let mut providers = registry(completions.clone(), Api::OpenaiCompletions);
    let chat = Model::new(
        "same",
        "Chat",
        Api::OpenaiCompletions,
        "account",
        "https://chat.example/v1",
    );
    let response = Model::new(
        "same",
        "Responses",
        Api::OpenaiResponses,
        "account",
        "https://responses.example/v1",
    );
    providers.register(
        Arc::new(HostProvider {
            models: vec![response.clone()],
            calls: responses.clone(),
        }),
        Api::OpenaiResponses,
    );
    providers.declare(ProviderDeclaration {
        id: "account".into(),
        models: vec![chat, response],
        api_key_env: None,
        requires_auth: false,
    });
    let mut selection = request();
    selection.api = Some(Api::OpenaiCompletions);
    let resolved = RuntimeConfig::default()
        .resolve(&selection, &providers, &FixedTime)
        .unwrap();
    assert_eq!(resolved.providers.len(), 1);
    for model in resolved.provider.models() {
        resolved
            .provider
            .stream_simple(
                model,
                &Context::new(vec![]),
                &SimpleStreamOptions::default(),
            )
            .await
            .result()
            .await
            .unwrap();
    }
    assert_eq!(completions.0.lock().unwrap().len(), 1);
    assert_eq!(responses.0.lock().unwrap().len(), 1);
    assert_eq!(
        completions.0.lock().unwrap()[0].0.api,
        Api::OpenaiCompletions
    );
    assert_eq!(responses.0.lock().unwrap()[0].0.api, Api::OpenaiResponses);
}

#[test]
fn config_expansion_uses_only_host_environment_and_disables_commands_by_default() {
    let environment = BTreeMap::from([("TOKEN".into(), "host-secret".into())]);
    let overlay = BTreeMap::from([("TOKEN".into(), "overlay-secret".into())]);
    assert_eq!(
        rpi_runtime::values::resolve_config_value(
            "Bearer ${TOKEN}",
            Some(&overlay),
            &environment,
            true
        )
        .as_deref(),
        Some("Bearer overlay-secret")
    );
    assert_eq!(
        rpi_runtime::values::resolve_config_value("$TOKEN", None, &environment, true).as_deref(),
        Some("host-secret")
    );
    assert!(
        rpi_runtime::values::resolve_config_value("!echo unsafe", None, &environment, true)
            .is_none()
    );
    assert!(RuntimeConfig {
        credentials: AuthStore::new(),
        ..Default::default()
    }
    .resolve(&request(), &ProviderRegistry::default(), &environment)
    .is_err());
}

#[tokio::test]
async fn url_credentials_stay_private_while_requests_keep_the_original_endpoint() {
    let calls = Arc::new(Calls::default());
    let providers = registry(calls.clone(), Api::OpenaiCompletions);
    let config = RuntimeConfig::default();
    let values = BTreeMap::from([("ACCOUNT_KEY".into(), "request-key".into())]);
    let mut selected = request();
    let endpoint = "https://private-user:private-password@host.example/v1?token=private-query#private-fragment";
    selected.base_url = Some(endpoint);
    let runtime = Runtime::new(&config, &providers, &values, &selected).unwrap();
    let public = serde_json::to_string(&runtime.snapshot()).unwrap();
    assert!(
        !public.contains("private-"),
        "URL credentials must not be published"
    );
    let current = runtime.current();
    assert_eq!(current.model.base_url, "https://host.example/v1");
    current
        .provider
        .stream_simple(
            &current.model,
            &Context::new(vec![]),
            &SimpleStreamOptions::default(),
        )
        .await
        .result()
        .await
        .unwrap();
    assert_eq!(calls.0.lock().unwrap()[0].0.base_url, endpoint);
}
