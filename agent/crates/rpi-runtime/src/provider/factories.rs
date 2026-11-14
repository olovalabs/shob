//! Built-in protocols and native extensions implement the same factory contract.
//! Authentication and model binding are applied once, outside every factory.
use super::binding::{ExecutionBinding, OAuthLease, RequestAuth};
use super::*;
use crate::config::Credential;
use crate::credentials::OAuthRefresher;
use crate::ProviderDeclaration as ProviderManifest;

trait ProviderFactory: Send + Sync {
    fn create(
        &self,
        provider_id: &str,
        models: Vec<Model>,
        settings: &crate::RuntimeSettings,
        values: &dyn ValueSource,
    ) -> Result<Arc<dyn Provider>, ResolveError>;
}

struct ProtocolFactory(rpi_ai::Api);
impl ProviderFactory for ProtocolFactory {
    fn create(
        &self,
        provider_id: &str,
        models: Vec<Model>,
        settings: &crate::RuntimeSettings,
        values: &dyn ValueSource,
    ) -> Result<Arc<dyn Provider>, ResolveError> {
        let fallback = if self.0 == rpi_ai::Api::AnthropicMessages {
            "https://api.anthropic.com"
        } else {
            "https://api.openai.com/v1"
        };
        let http = provider_http_client(&models, fallback, settings, values)?;
        let requires_auth = crate::catalog::builtin_provider(provider_id)
            .map(|provider| provider.requires_auth)
            .unwrap_or(true);
        let provider: Arc<dyn Provider> = match &self.0 {
            rpi_ai::Api::AnthropicMessages => {
                if !requires_auth {
                    Arc::new(AnthropicProvider::with_models_without_auth(http.clone(), models))
                } else {
                    Arc::new(AnthropicProvider::with_models_without_env_api_key(None, http.clone(), models))
                }
            }
            rpi_ai::Api::OpenaiCompletions => {
                if !requires_auth {
                    Arc::new(OpenAiCompletionsProvider::with_models_without_auth(
                        provider_id, http.clone(), models,
                    ))
                } else {
                    Arc::new(OpenAiCompletionsProvider::with_models_without_env_api_key(
                        provider_id, None, http.clone(), models,
                    ))
                }
            }
            rpi_ai::Api::OpenaiResponses | rpi_ai::Api::OpenaiCodexResponses => {
                if !requires_auth {
                    Arc::new(OpenAiResponsesProvider::with_models_without_auth(
                        provider_id, http.clone(), models,
                    ))
                } else {
                    Arc::new(OpenAiResponsesProvider::with_models_without_env_api_key(
                        provider_id, None, http.clone(), models,
                    ))
                }
            }
            rpi_ai::Api::AzureOpenaiResponses => Arc::new(
                rpi_ai::providers::azure::AzureProvider::with_models_without_env_api_key(
                    provider_id, None, http.clone(), models,
                ),
            ),
            rpi_ai::Api::GoogleGenerativeAi | rpi_ai::Api::GoogleVertex => Arc::new(
                rpi_ai::providers::google::GoogleProvider::with_models_without_env_api_key(
                    provider_id, None, http.clone(), models,
                ),
            ),
            rpi_ai::Api::BedrockConverseStream => Arc::new(
                rpi_ai::providers::bedrock::BedrockProvider::with_models_without_env_api_key(
                    provider_id, None, http.clone(), models,
                ),
            ),
            rpi_ai::Api::Other(api) if api == "google-gemini-cli" => Arc::new(
                rpi_ai::providers::google::GoogleProvider::with_models_without_env_api_key(
                    provider_id, None, http.clone(), models,
                ),
            ),
            rpi_ai::Api::Other(api) if api == "cursor-agent" => Arc::new(
                rpi_ai::providers::cursor::CursorProvider::with_models_without_env_api_key(
                    provider_id, None, http.clone(), models,
                ),
            ),
            rpi_ai::Api::Other(api) if api == "devin-agent" => Arc::new(
                rpi_ai::providers::devin::DevinProvider::with_models_without_env_api_key(
                    provider_id, None, http.clone(), models,
                ),
            ),
            rpi_ai::Api::Other(api) if api == "ollama-chat" => Arc::new(
                rpi_ai::providers::openai_completions::ollama::OllamaProvider::with_models_without_env_api_key(
                    provider_id, None, http.clone(), models,
                ),
            ),
            rpi_ai::Api::Other(api) if api == "openrouter" => {
                if !requires_auth {
                    Arc::new(OpenAiCompletionsProvider::with_models_without_auth(
                        provider_id, http.clone(), models,
                    ))
                } else {
                    Arc::new(OpenAiCompletionsProvider::with_models_without_env_api_key(
                        provider_id, None, http.clone(), models,
                    ))
                }
            }
            _ => unreachable!("only supported wire protocols are registered"),
        };
        if matches!(
            provider_id,
            "github-copilot"
                | "gitlab-duo"
                | "cloudflare-ai-gateway"
                | "alibaba-coding-plan"
                | "alibaba-token-plan"
                | "coreweave"
                | "snowflake"
        ) || self.0 == rpi_ai::Api::OpenaiCodexResponses
        {
            Ok(Arc::new(rpi_ai::providers::routed::RoutedProvider::new(
                provider, http,
            )))
        } else {
            Ok(provider)
        }
    }
}

struct ExtensionFactory(Arc<dyn Provider>);
impl ProviderFactory for ExtensionFactory {
    fn create(
        &self,
        _: &str,
        _: Vec<Model>,
        _: &crate::RuntimeSettings,
        _: &dyn ValueSource,
    ) -> Result<Arc<dyn Provider>, ResolveError> {
        Ok(self.0.clone())
    }
}

pub struct RuntimeFactories {
    protocols: BTreeMap<String, Arc<dyn ProviderFactory>>,
    providers: BTreeMap<(String, String), Arc<dyn ProviderFactory>>,
    pub manifests: Vec<ProviderManifest>,
    oauth_refresher: Option<Arc<dyn OAuthRefresher>>,
}

impl Default for RuntimeFactories {
    fn default() -> Self {
        let protocols = [
            rpi_ai::Api::AnthropicMessages,
            rpi_ai::Api::OpenaiCompletions,
            rpi_ai::Api::OpenaiResponses,
            rpi_ai::Api::OpenaiCodexResponses,
            rpi_ai::Api::AzureOpenaiResponses,
            rpi_ai::Api::GoogleGenerativeAi,
            rpi_ai::Api::GoogleVertex,
            rpi_ai::Api::BedrockConverseStream,
            rpi_ai::Api::Other("google-gemini-cli".into()),
            rpi_ai::Api::Other("cursor-agent".into()),
            rpi_ai::Api::Other("devin-agent".into()),
            rpi_ai::Api::Other("ollama-chat".into()),
            rpi_ai::Api::Other("openrouter".into()),
        ]
        .into_iter()
        .map(|api| {
            (
                api.as_str().to_string(),
                Arc::new(ProtocolFactory(api)) as Arc<dyn ProviderFactory>,
            )
        })
        .collect();
        Self {
            protocols,
            providers: BTreeMap::new(),
            manifests: Vec::new(),
            oauth_refresher: None,
        }
    }
}

impl RuntimeFactories {
    /// Add a declaration without loading an implementation (catalog discovery).
    pub fn declare(&mut self, declaration: ProviderManifest) {
        self.manifests.retain(|known| known.id != declaration.id);
        self.manifests.push(declaration);
    }
    /// Register a host-owned implementation. Its Arc keeps the original loader
    /// lease alive for all requests resolved from this registry.
    pub fn register(&mut self, provider: Arc<dyn Provider>, api: rpi_ai::Api) {
        let id = provider.id().to_string();
        self.providers.insert(
            (id, api.as_str().into()),
            Arc::new(ExtensionFactory(provider)),
        );
    }

    /// Renew stored OAuth credentials through `refresher` before requests send
    /// them. Without one, an expired stored OAuth credential is unavailable.
    pub fn set_oauth_refresher(&mut self, refresher: Arc<dyn OAuthRefresher>) {
        self.oauth_refresher = Some(refresher);
    }

    pub(crate) fn refreshes_oauth(&self, provider_id: &str) -> bool {
        self.oauth_refresher
            .as_ref()
            .is_some_and(|refresher| refresher.supports(provider_id))
    }

    /// A lease when `auth` sends `provider_id`'s stored, renewable OAuth token.
    pub(super) fn oauth_lease(
        &self,
        provider_id: &str,
        store: &config::AuthStore,
        auth: &RequestAuth,
    ) -> Option<OAuthLease> {
        let refresher = self
            .oauth_refresher
            .as_ref()
            .filter(|refresher| refresher.supports(provider_id))?;
        match store.get(provider_id) {
            Some(Credential::Oauth {
                access,
                refresh,
                expires,
            }) if *expires != 0 && !refresh.trim().is_empty() && auth.carries(access) => {
                Some(OAuthLease::new(Arc::clone(refresher), *expires))
            }
            _ => None,
        }
    }

    #[cfg(test)]
    pub(super) fn configure(
        &self,
        provider_id: &str,
        api: &rpi_ai::Api,
        models: Vec<Model>,
        auth: RequestAuth,
        settings: &crate::RuntimeSettings,
        values: &dyn ValueSource,
    ) -> Result<(Arc<dyn Provider>, Arc<ExecutionBinding>), ResolveError> {
        self.configure_with_oauth(provider_id, api, models, auth, None, settings, values)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn configure_with_oauth(
        &self,
        provider_id: &str,
        api: &rpi_ai::Api,
        models: Vec<Model>,
        auth: RequestAuth,
        oauth: Option<OAuthLease>,
        settings: &crate::RuntimeSettings,
        values: &dyn ValueSource,
    ) -> Result<(Arc<dyn Provider>, Arc<ExecutionBinding>), ResolveError> {
        let factory = self
            .providers
            .get(&(provider_id.into(), api.as_str().into()))
            .or_else(|| self.protocols.get(api.as_str()))
            .ok_or_else(|| ResolveError::UnsupportedApi {
                provider: provider_id.into(),
                api: api.as_str().into(),
            })?;
        let binding = Arc::new(ExecutionBinding::new(provider_id, models, auth, oauth));
        let inner = factory.create(
            provider_id,
            binding.descriptors().to_vec(),
            settings,
            values,
        )?;
        let provider = Arc::new(ConfiguredProvider {
            id: provider_id.into(),
            inner,
            binding: binding.clone(),
        });
        Ok((provider, binding))
    }
}

struct ConfiguredProvider {
    id: String,
    inner: Arc<dyn Provider>,
    binding: Arc<ExecutionBinding>,
}

/// The harness resolves providers by ID. Keep each ID unique even when a
/// configured account and an extension expose different wire protocols.
pub(super) fn combine_provider_runtimes(
    providers: Vec<Arc<dyn Provider>>,
) -> Vec<Arc<dyn Provider>> {
    let mut groups: Vec<Vec<Arc<dyn Provider>>> = Vec::new();
    for provider in providers {
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group[0].id() == provider.id())
        {
            group.push(provider);
        } else {
            groups.push(vec![provider]);
        }
    }
    groups
        .into_iter()
        .map(|mut routes| {
            if routes.len() == 1 {
                return routes.pop().unwrap();
            }
            Arc::new(RoutedProvider {
                id: routes[0].id().into(),
                models: routes
                    .iter()
                    .flat_map(|route| route.models().to_vec())
                    .collect(),
                routes,
            }) as Arc<dyn Provider>
        })
        .collect()
}

struct RoutedProvider {
    id: String,
    models: Vec<Model>,
    routes: Vec<Arc<dyn Provider>>,
}

#[async_trait::async_trait]
impl Provider for RoutedProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn models(&self) -> &[Model] {
        &self.models
    }
    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let route = self.routes.iter().find(|route| {
            route.models().iter().any(|candidate| {
                candidate.provider == model.provider
                    && candidate.api == model.api
                    && candidate.id.eq_ignore_ascii_case(&model.id)
            })
        });
        match route {
            Some(route) => route.stream_simple(model, context, options).await,
            None => unbound_model_error(model, &self.id).await,
        }
    }
}

async fn unbound_model_error(model: &Model, provider_id: &str) -> AssistantMessageEventStream {
    error_stream(
        model,
        provider_id,
        "model is not registered in this provider execution binding".into(),
    )
    .await
}

async fn error_stream(
    model: &Model,
    provider_id: &str,
    message: String,
) -> AssistantMessageEventStream {
    let (mut producer, stream) = rpi_ai::event_stream::create_assistant_message_event_stream();
    producer
        .push(rpi_ai::AssistantMessageEvent::Error {
            reason: rpi_ai::types::ErrorReason::Error,
            error: rpi_ai::types::AssistantMessage::terminal(
                model.api.clone(),
                provider_id,
                model.id.clone(),
                rpi_ai::types::StopReason::Error,
                message,
                0,
            ),
        })
        .await;
    stream
}

#[async_trait::async_trait]
impl Provider for ConfiguredProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn models(&self) -> &[Model] {
        self.binding.descriptors()
    }
    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        if !self.binding.binds(model) {
            return unbound_model_error(model, &self.id).await;
        }
        if let Err(error) = self.binding.renew_oauth(options).await {
            let id = &self.id;
            let message = format!(
                "The stored OAuth login for \"{id}\" has expired and could not be refreshed: {error}. Run `rpi auth login --provider {id}`."
            );
            return error_stream(model, id, message).await;
        }
        let Some((model, mut options)) = self.binding.prepare(model, options) else {
            return unbound_model_error(model, &self.id).await;
        };
        let explicit_auth = options.api_key.is_some()
            || [model.headers.as_ref(), options.headers.as_ref()]
                .into_iter()
                .flatten()
                .any(|headers| {
                    headers.keys().any(|name| {
                        [
                            "authorization",
                            "x-api-key",
                            "api-key",
                            "x-omp-aws-access-key-id",
                        ]
                        .iter()
                        .any(|auth| name.eq_ignore_ascii_case(auth))
                    })
                });
        if !explicit_auth
            && (self.id == "google-vertex"
                || matches!(
                    model.api,
                    rpi_ai::Api::GoogleVertex | rpi_ai::Api::BedrockConverseStream
                ))
        {
            match crate::cloud_auth::request_headers(&self.id).await {
                Ok(mut headers) => {
                    if let Some(overrides) = options.headers.take() {
                        for (name, value) in overrides {
                            rpi_ai::provider::set_http_header(&mut headers, &name, Some(&value));
                        }
                    }
                    options.headers = Some(headers);
                }
                Err(error) => return error_stream(&model, &self.id, error).await,
            }
        }
        self.inner.stream_simple(&model, context, &options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_ai::{
        types::{AssistantMessage, DoneReason, ErrorReason, StopReason},
        Api, AssistantMessageEvent,
    };
    use std::sync::Mutex;

    type Calls = Arc<Mutex<Vec<(Model, SimpleStreamOptions)>>>;
    struct RecordingFactory(Calls);
    struct RecordingProvider(Calls);
    impl ProviderFactory for RecordingFactory {
        fn create(
            &self,
            _: &str,
            models: Vec<Model>,
            _: &crate::RuntimeSettings,
            _: &dyn ValueSource,
        ) -> Result<Arc<dyn Provider>, ResolveError> {
            assert!(
                models.iter().all(|model| model.headers.is_none()),
                "factories receive public descriptors"
            );
            Ok(Arc::new(RecordingProvider(self.0.clone())))
        }
    }
    #[async_trait::async_trait]
    impl Provider for RecordingProvider {
        fn id(&self) -> &str {
            "recording"
        }
        fn models(&self) -> &[Model] {
            &[]
        }
        async fn stream_simple(
            &self,
            model: &Model,
            _: &Context,
            options: &SimpleStreamOptions,
        ) -> AssistantMessageEventStream {
            self.0
                .lock()
                .unwrap()
                .push((model.clone(), options.clone()));
            let (mut producer, stream) =
                rpi_ai::event_stream::create_assistant_message_event_stream();
            producer
                .push(AssistantMessageEvent::Done {
                    reason: DoneReason::Stop,
                    message: AssistantMessage::empty(
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

    struct TestRefresher {
        calls: std::sync::atomic::AtomicUsize,
        access: &'static str,
        expires: i64,
    }

    impl OAuthRefresher for TestRefresher {
        fn supports(&self, _: &str) -> bool {
            true
        }
        fn refresh(&self, _: &str) -> Result<crate::credentials::RefreshedOAuth, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(crate::credentials::RefreshedOAuth {
                access: self.access.into(),
                expires: self.expires,
            })
        }
    }

    fn oauth_provider(
        api: Api,
        auth: RequestAuth,
        expires: i64,
        refresher: Arc<TestRefresher>,
        calls: Calls,
    ) -> Arc<dyn Provider> {
        let mut factories = RuntimeFactories::default();
        factories
            .protocols
            .insert(api.as_str().into(), Arc::new(RecordingFactory(calls)));
        let model = Model::new(
            "model",
            "Model",
            api.clone(),
            "account",
            "https://configured.example/v1",
        );
        factories
            .configure_with_oauth(
                "account",
                &api,
                vec![model],
                auth,
                Some(OAuthLease::new(refresher, expires)),
                &crate::RuntimeSettings::default(),
                &BTreeMap::<String, String>::new(),
            )
            .unwrap()
            .0
    }

    async fn request(
        provider: &Arc<dyn Provider>,
        options: &SimpleStreamOptions,
    ) -> AssistantMessage {
        provider
            .stream_simple(&provider.models()[0], &Context::new(vec![]), options)
            .await
            .result()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn oauth_anthropic_overrides_and_invalid_refresh_never_reach_provider() {
        let calls = Calls::default();
        let refresher = Arc::new(TestRefresher {
            calls: 0.into(),
            access: " ",
            expires: 0,
        });
        let provider = oauth_provider(
            Api::AnthropicMessages,
            RequestAuth::Headers(BTreeMap::from([(
                "authorization".into(),
                "Bearer old-secret".into(),
            )])),
            1,
            refresher.clone(),
            calls.clone(),
        );
        let key = SimpleStreamOptions {
            api_key: Some("explicit-key".into()),
            ..Default::default()
        };
        request(&provider, &key).await;
        let header = SimpleStreamOptions {
            headers: Some(BTreeMap::from([(
                "Authorization".into(),
                "Bearer explicit".into(),
            )])),
            ..Default::default()
        };
        request(&provider, &header).await;
        assert_eq!(refresher.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let result = request(&provider, &SimpleStreamOptions::default()).await;
        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(refresher.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0].1.api_key.as_deref(), Some("explicit-key"));
        assert_eq!(
            recorded[1].1.headers.as_ref().unwrap()["Authorization"],
            "Bearer explicit"
        );
    }

    #[tokio::test]
    async fn oauth_renews_anthropic_bearer_header() {
        let calls = Calls::default();
        let refresher = Arc::new(TestRefresher {
            calls: 0.into(),
            access: "renewed-secret",
            expires: 0,
        });
        let provider = oauth_provider(
            Api::AnthropicMessages,
            RequestAuth::Headers(BTreeMap::from([(
                "authorization".into(),
                "Bearer old-secret".into(),
            )])),
            1,
            refresher,
            calls.clone(),
        );
        request(&provider, &SimpleStreamOptions::default()).await;
        assert_eq!(
            calls.lock().unwrap()[0].1.headers.as_ref().unwrap()["authorization"],
            "Bearer renewed-secret"
        );
        assert!(!serde_json::to_string(provider.models())
            .unwrap()
            .contains("secret"));
    }

    #[tokio::test]
    async fn one_provider_routes_duplicate_model_ids_by_api_with_private_credentials() {
        let calls = Calls::default();
        let mut factories = RuntimeFactories::default();
        for api in [Api::OpenaiCompletions, Api::OpenaiResponses] {
            factories.protocols.insert(
                api.as_str().into(),
                Arc::new(RecordingFactory(calls.clone())),
            );
        }
        let mut runtimes = Vec::new();
        for (api, token) in [
            (Api::OpenaiCompletions, "completions-token"),
            (Api::OpenaiResponses, "responses-token"),
        ] {
            let mut model = Model::new(
                "same-model",
                "Same model",
                api.clone(),
                "deepseek",
                format!("https://configured.example/{}", api.as_str()),
            );
            model.headers = Some(BTreeMap::from([("x-route".into(), token.into())]));
            runtimes.push(
                factories
                    .configure(
                        "deepseek",
                        &api,
                        vec![model],
                        RequestAuth::ApiKey(token.into()),
                        &crate::RuntimeSettings::default(),
                        &BTreeMap::<String, String>::new(),
                    )
                    .unwrap()
                    .0,
            );
        }
        let providers = combine_provider_runtimes(runtimes);
        assert_eq!(providers.len(), 1);
        let provider = &providers[0];
        assert_eq!(provider.models().len(), 2);
        assert!(provider
            .models()
            .iter()
            .all(|model| model.headers.is_none()));
        // This is the same ID-only lookup used by the harness after /provider.
        for model in provider.models() {
            let selected = providers
                .iter()
                .find(|provider| provider.id() == model.provider)
                .unwrap();
            let mut requested = model.clone();
            requested.id = requested.id.to_uppercase();
            requested.base_url = "https://untrusted.example".into();
            selected
                .stream_simple(
                    &requested,
                    &Context::new(vec![]),
                    &SimpleStreamOptions::default(),
                )
                .await
                .result()
                .await
                .unwrap();
        }
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 2);
        for (call, (api, token)) in recorded.iter().zip([
            (Api::OpenaiCompletions, "completions-token"),
            (Api::OpenaiResponses, "responses-token"),
        ]) {
            assert_eq!(call.0.api, api);
            assert_eq!(
                call.0.base_url,
                format!("https://configured.example/{}", api.as_str())
            );
            assert_eq!(call.0.headers.as_ref().unwrap()["x-route"], token);
            assert_eq!(call.1.api_key.as_deref(), Some(token));
        }
        drop(recorded);
        for (provider_id, api, id) in [
            ("other", Api::OpenaiCompletions, "same-model"),
            ("deepseek", Api::AnthropicMessages, "same-model"),
            ("deepseek", Api::OpenaiResponses, "unknown-model"),
        ] {
            let requested = Model::new(id, id, api, provider_id, "https://untrusted.example");
            let result = provider
                .stream_simple(
                    &requested,
                    &Context::new(vec![]),
                    &SimpleStreamOptions::default(),
                )
                .await
                .result()
                .await
                .unwrap();
            assert_eq!(result.stop_reason, StopReason::Error);
        }
        assert_eq!(calls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn extension_factory_only_handles_its_registered_wire_api() {
        let extension_calls = Calls::default();
        let protocol_calls = Calls::default();
        let mut factories = RuntimeFactories::default();
        factories.providers.insert(
            ("deepseek".into(), "openai-completions".into()),
            Arc::new(RecordingFactory(extension_calls.clone())),
        );
        factories.protocols.insert(
            "openai-responses".into(),
            Arc::new(RecordingFactory(protocol_calls.clone())),
        );
        for api in [Api::OpenaiCompletions, Api::OpenaiResponses] {
            let model = Model::new(
                "same-model",
                "Same model",
                api.clone(),
                "deepseek",
                "https://configured.example/v1",
            );
            let (provider, _) = factories
                .configure(
                    "deepseek",
                    &api,
                    vec![model],
                    RequestAuth::None,
                    &crate::RuntimeSettings::default(),
                    &BTreeMap::<String, String>::new(),
                )
                .unwrap();
            provider
                .stream_simple(
                    &provider.models()[0],
                    &Context::new(vec![]),
                    &SimpleStreamOptions::default(),
                )
                .await
                .result()
                .await
                .unwrap();
        }
        assert_eq!(extension_calls.lock().unwrap().len(), 1);
        assert_eq!(
            extension_calls.lock().unwrap()[0].0.api,
            Api::OpenaiCompletions
        );
        assert_eq!(protocol_calls.lock().unwrap().len(), 1);
        assert_eq!(
            protocol_calls.lock().unwrap()[0].0.api,
            Api::OpenaiResponses
        );
    }

    #[tokio::test]
    async fn protocol_and_extension_factories_use_the_same_private_binding() {
        for extension in [false, true] {
            let calls = Calls::default();
            let mut factories = RuntimeFactories::default();
            let factory = Arc::new(RecordingFactory(calls.clone())) as Arc<dyn ProviderFactory>;
            if extension {
                factories
                    .providers
                    .insert(("account".into(), "openai-completions".into()), factory);
            } else {
                factories
                    .protocols
                    .insert("openai-completions".into(), factory);
            }
            let mut raw = Model::new(
                "model",
                "Model",
                Api::OpenaiCompletions,
                "account",
                "https://configured.example/v1",
            );
            raw.headers = Some(BTreeMap::from([(
                "x-api-key".into(),
                "private-header".into(),
            )]));
            let (provider, binding) = factories
                .configure(
                    "account",
                    &raw.api.clone(),
                    vec![raw],
                    RequestAuth::ApiKey("private-credential".into()),
                    &crate::RuntimeSettings::default(),
                    &BTreeMap::<String, String>::new(),
                )
                .unwrap();
            let serialized = serde_json::to_string(provider.models()).unwrap();
            assert!(
                !serialized.contains("private-header")
                    && !serialized.contains("private-credential")
            );
            assert!(!format!("{binding:?}").contains("private-credential"));
            let mut requested = provider.models()[0].clone();
            requested.base_url = "https://untrusted.example/v1".into();
            requested.headers = Some(BTreeMap::from([(
                "authorization".into(),
                "injected".into(),
            )]));
            provider
                .stream_simple(
                    &requested,
                    &Context::new(vec![]),
                    &SimpleStreamOptions::default(),
                )
                .await
                .result()
                .await
                .unwrap();
            let recorded = calls.lock().unwrap().clone();
            assert_eq!(recorded.len(), 1);
            assert_eq!(recorded[0].0.base_url, "https://configured.example/v1");
            assert_eq!(
                recorded[0].0.headers.as_ref().unwrap()["x-api-key"],
                "private-header"
            );
            assert_eq!(recorded[0].1.api_key.as_deref(), Some("private-credential"));
            assert!(!recorded[0]
                .0
                .headers
                .as_ref()
                .unwrap()
                .contains_key("authorization"));
            drop(recorded);
            requested.provider = "another-account".into();
            let mut rejected = provider
                .stream_simple(
                    &requested,
                    &Context::new(vec![]),
                    &SimpleStreamOptions::default(),
                )
                .await;
            assert!(matches!(
                rejected.next().await,
                Some(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    ..
                })
            ));
            assert_eq!(
                rejected.result().await.unwrap().stop_reason,
                StopReason::Error
            );
            assert_eq!(calls.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn request_overrides_and_model_headers_remain_scoped_to_their_execution() {
        let calls = Calls::default();
        let mut factories = RuntimeFactories::default();
        factories.providers.insert(
            ("gateway".into(), "anthropic-messages".into()),
            Arc::new(RecordingFactory(calls.clone())),
        );
        let mut models = vec![];
        for (id, token) in [("first", "first-token"), ("second", "second-token")] {
            let mut model = Model::new(
                id,
                id,
                Api::AnthropicMessages,
                "gateway",
                "https://gateway.example",
            );
            model.headers = Some(BTreeMap::from([("x-api-key".into(), token.into())]));
            models.push(model);
        }
        let (provider, _) = factories
            .configure(
                "gateway",
                &Api::AnthropicMessages,
                models,
                RequestAuth::Headers(BTreeMap::from([(
                    "authorization".into(),
                    "Bearer default".into(),
                )])),
                &crate::RuntimeSettings::default(),
                &BTreeMap::<String, String>::new(),
            )
            .unwrap();
        let options = SimpleStreamOptions {
            headers: Some(BTreeMap::from([(
                "Authorization".into(),
                "Bearer per-request".into(),
            )])),
            ..Default::default()
        };
        for model in provider.models() {
            provider
                .stream_simple(model, &Context::new(vec![]), &options)
                .await
                .result()
                .await
                .unwrap();
        }
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(
            recorded[0].0.headers.as_ref().unwrap()["x-api-key"],
            "first-token"
        );
        assert_eq!(
            recorded[1].0.headers.as_ref().unwrap()["x-api-key"],
            "second-token"
        );
        assert_eq!(recorded[0].1.headers.as_ref().unwrap().len(), 1);
        assert_eq!(
            recorded[0].1.headers.as_ref().unwrap()["Authorization"],
            "Bearer per-request"
        );
    }
}
