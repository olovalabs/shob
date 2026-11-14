//! Model catalog merging, provider-scoped selection and private execution binding.
//!
//! Hosts supply all configuration and environment access through RuntimeConfig
//! and ValueSource. Credential precedence, exact model matching, saved defaults
//! and provider/API routing are shared by embedded apps and the CLI adapter.

use std::collections::BTreeMap;
use std::sync::Arc;

use rpi_ai::providers::anthropic::models::anthropic_models;
use rpi_ai::providers::anthropic::AnthropicProvider;
use rpi_ai::providers::openai_completions::OpenAiCompletionsProvider;
use rpi_ai::providers::openai_responses::openai_responses_models;
use rpi_ai::providers::openai_responses::OpenAiResponsesProvider;
use rpi_ai::{
    AssistantMessageEventStream, Context, Model, Provider, SimpleStreamOptions, ThinkingLevel,
};

mod binding;
mod factories;
use binding::{ExecutionBinding, RequestAuth};
pub use factories::RuntimeFactories;

#[cfg(test)]
use crate::config::Credential;
use crate::config::{self, DEFAULT_PROVIDER_ID};
use crate::thinking::parse_thinking_level;
use crate::{RuntimeSettings, ValueSource};

/// The default Anthropic model when `--model` is absent. Kept in sync with the
/// current upstream `defaultModelPerProvider.anthropic` entry.
pub const DEFAULT_MODEL_ID: &str = "claude-opus-4-8";

/// Core provider defaults, checked before the first authenticated catalog entry.
/// Other providers own their default catalog ordering through their package.
const DEFAULT_MODELS_PER_PROVIDER: &[(&str, &str)] =
    &[("anthropic", DEFAULT_MODEL_ID), ("openai", "gpt-6-astra")];

/// The default thinking level when neither `--thinking` nor a `:level` suffix
/// is present. Mirrors the TS `DEFAULT_THINKING_LEVEL` (`"medium"`, clamped to
/// model capabilities by the harness's provider build_params).
pub const DEFAULT_THINKING_LEVEL: ThinkingLevel = ThinkingLevel::Medium;

/// The resolved run configuration: the provider handle, the chosen model, and
/// the effective thinking level (after `--thinking` / `:level` / model-clamp).
#[derive(Clone)]
pub struct ResolvedModel {
    /// The selected provider. Cheap to clone through the trait-object `Arc`.
    pub provider: Arc<dyn Provider>,
    /// The chosen model from the catalog.
    pub model: Model,
    /// Effective thinking level (the requested level, before model-clamp — the
    /// harness/provider clamps to the model's supported set).
    pub thinking_level: ThinkingLevel,
    /// Whether the selected runtime is configured with a provider-default key
    /// or an explicit anonymous policy. Otherwise auth belongs to model headers.
    ///
    /// Retained for diagnostics and API compatibility. Published catalogs are
    /// already filtered when their execution bindings are constructed.
    pub has_provider_key: bool,
    /// Saved theme name from `~/.rpi/agent/settings.json`, if any. Best-effort:
    /// the TUI applies it at startup when it matches a known preset
    /// (dark/light/monochrome); otherwise ignored.
    pub theme: Option<String>,
    /// Every authenticated provider runtime, with the selected provider first.
    /// The harness resolves a provider lazily by `model.provider`, so this lets
    /// `/model` hot-switch across providers without a restart.
    pub providers: Vec<Arc<dyn Provider>>,
    /// Compatibility diagnostics for the caller to display through its UI.
    /// Resolution and catalog queries never write directly to the terminal.
    pub compat_warnings: Vec<String>,
    /// Private state shared with the selected configured provider.
    execution: Arc<ExecutionBinding>,
    pub(crate) snapshot_catalog: Vec<Model>,
    pub(crate) snapshot_auth: BTreeMap<String, crate::credentials::AuthState>,
}

impl std::fmt::Debug for ResolvedModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedModel")
            .field("provider", &self.provider.id())
            .field("model", &self.model.id)
            .field("thinking_level", &self.thinking_level)
            .field("has_provider_key", &self.has_provider_key)
            .field("theme", &self.theme)
            .finish()
    }
}

/// The env var consulted for the API key. Mirrors TS `ANTHROPIC_API_KEY`.
pub const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";

/// The env var consulted for a bearer token (routed as
/// `Authorization: Bearer`). Mirrors TS `ANTHROPIC_AUTH_TOKEN` — used by
/// third-party Anthropic-compatible gateways (one-api/new-api/claude-code-router
/// and private reverse proxies) that authenticate via `Authorization` rather
/// than `x-api-key`.
pub const ANTHROPIC_AUTH_TOKEN_ENV: &str = "ANTHROPIC_AUTH_TOKEN";

/// The env var that overrides the Anthropic endpoint base URL. Mirrors TS
/// `ANTHROPIC_BASE_URL` — point this at a gateway/proxy that speaks the
/// `/v1/messages` protocol.
pub const ANTHROPIC_BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";

/// Standard OpenAI API-key environment variable used by the
/// `openai-completions` provider.
pub const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";

/// Hint text surfaced when no credential source is available. Lists every
/// accepted source so the user can pick the one that fits their setup.
pub const NO_API_KEY_HINT: &str =
    "models.json apiKey, OPENAI_API_KEY / ANTHROPIC_API_KEY / ANTHROPIC_AUTH_TOKEN env, --api-key, or `rpi auth login`";

/// A resolution error. The TS resolver returns `{ error, warning }`; v1 folds
/// both into a single enum since the CLI treats them the same (print + non-zero
/// exit) except `NoApiKey`, which prints guidance then exits.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("Invalid provider HTTP/proxy configuration")]
    HttpConfiguration,
    #[error("No implementation registered for provider {provider:?} and API {api}")]
    UnsupportedApi { provider: String, api: String },
    #[error("Unknown provider \"{0}\". Supported: anthropic, openai-completions, openai-responses, a models.json provider id, or an installed provider package")]
    UnknownProvider(String),
    #[error(
        "Provider \"{requested}\" is ambiguous: {matches}. Use the exact provider id to select one."
    )]
    AmbiguousProvider { requested: String, matches: String },
    #[error("No model matches \"{pattern}\". Available: {available}")]
    NoMatch { pattern: String, available: String },
    #[error(
        "Model \"{pattern}\" is ambiguous across providers: {matches}. {auth_hint} Use --provider or provider/model."
    )]
    AmbiguousModel {
        pattern: String,
        matches: String,
        auth_hint: &'static str,
    },
    #[error("Invalid thinking level \"{0}\" in model pattern. Valid: {1}")]
    InvalidThinkingLevel(String, String),
    #[error("No API key. Set one of: {hint}")]
    NoApiKey { hint: &'static str },
    #[error("Could not read config: {0}")]
    Config(#[from] config::ConfigError),
}

/// Validate configured endpoints and build a client whose proxy routing uses
/// each request URL, including NO_PROXY and scheme-specific proxy settings.
pub(crate) fn provider_http_client(
    models: &[Model],
    default_base_url: &str,
    settings: &RuntimeSettings,
    values: &dyn ValueSource,
) -> Result<reqwest::Client, ResolveError> {
    // Explicit entries prevent proxy lookup from falling back to process env.
    // Disable reqwest's own implicit system proxy discovery as well.
    let env: std::collections::HashMap<String, String> = [
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
    ]
    .into_iter()
    .map(|name| {
        (
            name.into(),
            values
                .env(name)
                .or_else(|| values.env(&name.to_ascii_uppercase()))
                .unwrap_or_default(),
        )
    })
    .collect();
    let builder = reqwest::Client::builder().no_proxy();
    // A redirect can leave the configured NO_PROXY hosts. Validate both proxy
    // routes independently of those hosts, unless every URL is explicitly
    // bypassed. Callback errors must not silently become direct connections.
    let bypass_all = env["no_proxy"]
        .split([',', ' ', '\t', '\n'])
        .any(|host| host == "*");
    if !bypass_all {
        let mut policy = env.clone();
        policy.insert("no_proxy".into(), String::new());
        for target in [
            "http://proxy-validation.invalid",
            "https://proxy-validation.invalid",
        ] {
            rpi_ai::http::resolve_http_proxy_url_for_target(target, Some(&policy))
                .map_err(|_| ResolveError::HttpConfiguration)?;
        }
    }
    // Validate endpoints before creating a usable provider. Error text must
    // never include proxy URLs, which can contain usernames and passwords.
    let targets: Vec<_> = if models.is_empty() {
        vec![default_base_url]
    } else {
        models
            .iter()
            .map(|model| {
                if model.base_url.is_empty() {
                    default_base_url
                } else {
                    model.base_url.as_str()
                }
            })
            .collect()
    };
    for target in targets {
        rpi_ai::http::resolve_http_proxy_url_for_target(target, Some(&env))
            .map_err(|_| ResolveError::HttpConfiguration)?;
    }
    let mut builder = builder.proxy(reqwest::Proxy::custom(move |url| {
        rpi_ai::http::resolve_http_proxy_url_for_target(url.as_str(), Some(&env))
            .ok()
            .flatten()
    }));
    let timeout = settings
        .http_idle_timeout_ms
        .unwrap_or(rpi_ai::http::DEFAULT_HTTP_IDLE_TIMEOUT_MS);
    builder = builder.pool_idle_timeout(if timeout > 0 {
        Some(std::time::Duration::from_millis(timeout as u64))
    } else {
        None
    });
    builder.build().map_err(|_| ResolveError::HttpConfiguration)
}

pub(crate) fn resolve_with_factories(
    request: &crate::ModelRequest<'_>,
    inputs: &crate::RuntimeConfig,
    factories: &RuntimeFactories,
    values: &dyn ValueSource,
) -> Result<ResolvedModel, ResolveError> {
    let resolution_values = crate::values::ResolutionValues::new(values);
    let values: &dyn ValueSource = &resolution_values;
    let cli_provider = request.provider;
    let cli_model = request.model;
    let cli_thinking = request.thinking;
    let cli_api_key = request.api_key;
    let cli_base_url = request.base_url;
    let settings = &inputs.settings;
    let mut models_cfg = inputs.models.clone();
    let auth_store = &inputs.credentials;
    // Load models/auth once, then resolve Anthropic-wire credentials per
    // provider id. A key belonging to `anthropic` must never make an unrelated
    // gateway available or become that gateway's provider default.
    apply_manifest_defaults(&mut models_cfg, &factories.manifests);
    let compat_warnings = config::model_compat_warnings(&models_cfg);
    let canonical_cli_provider = cli_provider
        .map(|requested| canonicalize_cli_provider(requested, &models_cfg))
        .transpose()?;
    let cli_provider = canonical_cli_provider.as_deref();
    let refreshable = |provider: &str| factories.refreshes_oauth(provider);
    let anthropic_credentials = resolve_anthropic_credentials_with(
        &models_cfg,
        auth_store,
        &factories.manifests,
        values,
        &refreshable,
    );
    let openai_credentials = resolve_openai_credentials_with(
        &models_cfg,
        auth_store,
        &factories.manifests,
        values,
        &refreshable,
    );
    // The CLI override is intentionally not inserted into either provider map:
    // it is applied only after the final model/provider identity is selected.
    let cli_api_key = cli_api_key
        .filter(|key| !key.is_empty())
        .map(str::to_string);

    // ---- Endpoint override (--base-url → ANTHROPIC_BASE_URL) ----
    let cli_base_url_override = cli_base_url.map(str::to_string);
    let anthropic_base_url_override = values
        .env(ANTHROPIC_BASE_URL_ENV)
        .filter(|value| !value.is_empty());

    // ---- Catalog: built-in + ~/.rpi/models.json (merged, reusing the
    // already-loaded config) ----
    let mut catalog = build_catalog(&models_cfg, &factories.manifests, values);

    // The ambient Anthropic override belongs only to the built-in identity.
    // Apply the CLI override after selection so other providers keep their
    // own endpoints and credentials when switched to via `/model`.
    for model in &mut catalog {
        if matches!(model.api, rpi_ai::Api::AnthropicMessages)
            && model.provider == DEFAULT_PROVIDER_ID
        {
            if let Some(base) = &anthropic_base_url_override {
                model.base_url = base.clone();
            }
        }
    }

    // Snapshot the full (un-retained) catalog so auxiliary provider runtimes
    // for `/model` hot-switching can be built after the selected one.
    let mut full_catalog = catalog.clone();
    if cli_model.is_none() {
        catalog.retain(crate::catalog::is_chat_model);
    }

    if let Some(api) = &request.api {
        catalog.retain(|model| &model.api == api);
    }

    if let Some(requested) = cli_provider {
        catalog.retain(|model| provider_matches(model, requested, &models_cfg));
    }

    let available = catalog
        .iter()
        .map(|m| m.id.clone())
        .collect::<Vec<_>>()
        .join(", ");
    if catalog.is_empty() {
        return Err(ResolveError::NoMatch {
            pattern: cli_provider.unwrap_or("default").to_string(),
            available,
        });
    }

    // ---- Model selection ----
    // With `--model`: parse the pattern (`provider/id[:thinking]`), match it
    // exactly against the catalog (TS fuzzy/partial match is a deliberate v1
    // omission — see module docs §5). Without `--model`: pi `findInitialModel`
    // precedence — (3) the saved default from settings (when present + authed),
    // then (4) `pick_default_model` (built-in default if authed, else first
    // authed). The saved default mirrors `findInitialModel` step 3 and lets a
    // copied pi `settings.json`'s `defaultModel` come alive on launch.
    let (mut model, thinking_level) = match cli_model {
        Some(raw) => {
            let parsed_pattern = split_model_pattern(raw, cli_provider, &catalog, &models_cfg)?;
            if let Some(provider) = parsed_pattern.provider.as_deref() {
                if !provider_is_known(provider, &models_cfg) {
                    return Err(ResolveError::UnknownProvider(provider.to_string()));
                }
            }
            let mut pattern_thinking = parsed_pattern.thinking;
            let mut selected = find_cli_model(
                &parsed_pattern.model_id,
                parsed_pattern.provider.as_deref(),
                &catalog,
                &models_cfg,
                &anthropic_credentials,
                &openai_credentials,
            )?;

            // A vendor may include its own name in the actual wire model id
            // (OpenRouter's `openrouter/auto`). If the stripped id is absent,
            // retry the full id even when --provider was explicit.
            if selected.is_none() && !parsed_pattern.inferred_provider {
                selected = find_cli_model(
                    &parsed_pattern.raw_model_id,
                    cli_provider,
                    &catalog,
                    &models_cfg,
                    &anthropic_credentials,
                    &openai_credentials,
                )?;
                if selected.is_some() {
                    pattern_thinking = None;
                }
            }
            if parsed_pattern.inferred_provider {
                if let Some(inferred) = selected.as_ref() {
                    if !model_is_authed_for_resolution(
                        inferred,
                        &anthropic_credentials,
                        &openai_credentials,
                        false,
                    ) {
                        let authenticated_raw_matches = |candidate: &str| {
                            catalog
                                .iter()
                                .filter(|model| {
                                    model.id.eq_ignore_ascii_case(candidate)
                                        && (model.provider != inferred.provider
                                            || model.id != inferred.id)
                                        && model_is_authed_for_resolution(
                                            model,
                                            &anthropic_credentials,
                                            &openai_credentials,
                                            false,
                                        )
                                })
                                .collect::<Vec<_>>()
                        };
                        let mut raw_matches =
                            authenticated_raw_matches(&parsed_pattern.raw_model_id);
                        let mut matched_complete_raw_id = true;
                        if raw_matches.is_empty() && parsed_pattern.thinking.is_some() {
                            if let Some((raw_without_thinking, _)) =
                                parsed_pattern.raw_model_id.rsplit_once(':')
                            {
                                raw_matches = authenticated_raw_matches(raw_without_thinking);
                                matched_complete_raw_id = false;
                            }
                        }
                        if let [raw_match] = raw_matches.as_slice() {
                            selected = Some((*raw_match).clone());
                            if matched_complete_raw_id {
                                pattern_thinking = None;
                            }
                        }
                    }
                } else {
                    selected = find_cli_model(
                        &parsed_pattern.raw_model_id,
                        None,
                        &catalog,
                        &models_cfg,
                        &anthropic_credentials,
                        &openai_credentials,
                    )?;
                    if selected.is_some() {
                        pattern_thinking = None;
                    } else if parsed_pattern.thinking.is_some() {
                        if let Some((raw_without_thinking, _)) =
                            parsed_pattern.raw_model_id.rsplit_once(':')
                        {
                            selected = find_cli_model(
                                raw_without_thinking,
                                None,
                                &catalog,
                                &models_cfg,
                                &anthropic_credentials,
                                &openai_credentials,
                            )?;
                        }
                    }
                }
            }

            let model = match selected {
                Some(m) => m,
                None => {
                    return Err(ResolveError::NoMatch {
                        pattern: parsed_pattern.model_id,
                        available,
                    });
                }
            };
            // `--thinking` wins over a parsed `:level` suffix; an exact model
            // id containing that suffix does not implicitly set thinking.
            let thinking_level = cli_thinking
                .or(pattern_thinking)
                .unwrap_or(DEFAULT_THINKING_LEVEL);
            (model, thinking_level)
        }
        None => {
            // `--thinking` > settings `defaultThinkingLevel` > built-in default.
            // The settings level is honored only when its model is also the
            // saved default (matches pi, which applies `defaultThinkingLevel`
            // inside the step-3 branch). For the fallback default, keep
            // `DEFAULT_THINKING_LEVEL`.
            let settings_thinking = settings
                .default_thinking_level
                .as_deref()
                .and_then(parse_thinking_level);

            // (3) Saved default from settings, when the provider is anthropic
            // (or absent — v1 is anthropic-only) OR names a configured
            // models.json gateway (config-namespacing: the saved
            // `defaultProvider` id matches a `~/.rpi/models.json` provider
            // key), and the saved model is authed. Without the gateway arm a
            // copied pi settings.json (`defaultProvider:
            // "cc-switch-deep-seek-copy-2"`) is ignored and the default falls
            // to first-authed — which, once a second gateway is enabled, may
            // NOT be the user's saved choice (BTreeMap provider order).
            let saved_provider = settings
                .default_provider
                .as_deref()
                .filter(|provider| provider_is_known(provider, &models_cfg));
            let saved = settings.default_model.as_deref().and_then(|id| {
                if settings.default_provider.is_some() && saved_provider.is_none() {
                    return None;
                }
                find_model(id, saved_provider, &catalog, &models_cfg).filter(|m| {
                    model_is_authed_for_resolution(
                        m,
                        &anthropic_credentials,
                        &openai_credentials,
                        cli_api_key.is_some(),
                    )
                })
            });
            if let Some(model) = saved {
                let thinking_level = cli_thinking
                    .or(settings_thinking)
                    .unwrap_or(DEFAULT_THINKING_LEVEL);
                (model, thinking_level)
            } else {
                // (4) Fallback: built-in default if authed, else first authed.
                let thinking_level = cli_thinking.unwrap_or(DEFAULT_THINKING_LEVEL);
                let model = pick_default_model(
                    &catalog,
                    &models_cfg,
                    &anthropic_credentials,
                    &openai_credentials,
                    cli_api_key.is_some(),
                );
                (model, thinking_level)
            }
        }
    };

    // ---- Provider build ----
    let selected_api = model.api.clone();
    let selected_provider = model.provider.clone();
    if !crate::catalog::is_chat_model(&model) {
        let kind = crate::catalog::model_metadata(&model.provider, &model.id)
            .map(|metadata| metadata.kind.as_str())
            .unwrap_or("non-chat");
        return Err(ResolveError::UnsupportedApi {
            provider: selected_provider.clone(),
            api: format!("{kind} task via {}", selected_api.as_str()),
        });
    }
    if let Some(base) = &cli_base_url_override {
        model.base_url = base.clone();
        for candidate in &mut full_catalog {
            if candidate.provider == selected_provider && candidate.api == selected_api {
                candidate.base_url = base.clone();
            }
        }
    }
    let selected_anthropic_credential = if matches!(selected_api, rpi_ai::Api::AnthropicMessages) {
        let auth_header = models_cfg
            .providers
            .get(&selected_provider)
            .and_then(|provider| provider.auth_header)
            .unwrap_or(false);
        credential_for_selected_anthropic_provider(
            cli_api_key.as_deref(),
            &anthropic_credentials,
            &selected_provider,
            auth_header,
        )
    } else {
        None
    };
    let selected_openai_key = if selected_api != rpi_ai::Api::AnthropicMessages {
        cli_api_key
            .clone()
            .or_else(|| openai_credential_for(&openai_credentials, &selected_provider).cloned())
    } else {
        None
    };
    let selected_is_authed = openai_credentials.contains_key(&selected_provider)
        || model_has_header_auth(&model)
        || match selected_api {
            rpi_ai::Api::AnthropicMessages => selected_anthropic_credential.is_some(),
            rpi_ai::Api::OpenaiCompletions | rpi_ai::Api::OpenaiResponses => {
                selected_openai_key.is_some()
            }
            _ => false,
        };
    if !selected_is_authed {
        return Err(ResolveError::NoApiKey {
            hint: NO_API_KEY_HINT,
        });
    }

    let (provider, execution, has_provider_key) = build_runtime(
        &selected_provider,
        &selected_api,
        &full_catalog,
        (
            selected_anthropic_credential.as_ref(),
            selected_openai_key.as_deref(),
        ),
        cli_api_key.is_none().then_some(auth_store),
        settings,
        values,
        factories,
    )?;

    // Build runtimes for every OTHER authenticated provider so the harness can
    // hot-switch providers by `model.provider` (its lazy resolve matches by
    // id). The selected provider is already built above.
    let mut providers: Vec<Arc<dyn Provider>> = vec![provider.clone()];
    {
        let mut seen: Vec<(String, rpi_ai::Api)> =
            vec![(selected_provider.clone(), selected_api.clone())];
        for candidate in &full_catalog {
            if !crate::catalog::is_chat_model(candidate) {
                continue;
            }
            let key = (candidate.provider.clone(), candidate.api.clone());
            if seen.contains(&key) {
                continue;
            }
            seen.push(key.clone());
            let (anth, openai_key) = match &candidate.api {
                rpi_ai::Api::AnthropicMessages => {
                    let auth_header = models_cfg
                        .providers
                        .get(&candidate.provider)
                        .and_then(|p| p.auth_header)
                        .unwrap_or(false);
                    (
                        credential_for_selected_anthropic_provider(
                            None,
                            &anthropic_credentials,
                            &candidate.provider,
                            auth_header,
                        ),
                        None,
                    )
                }
                rpi_ai::Api::OpenaiCompletions | rpi_ai::Api::OpenaiResponses => (
                    None,
                    openai_credential_for(&openai_credentials, &candidate.provider).cloned(),
                ),
                _ => (
                    None,
                    openai_credential_for(&openai_credentials, &candidate.provider).cloned(),
                ),
            };
            let authed = openai_credentials.contains_key(&candidate.provider)
                || model_has_header_auth(candidate)
                || match candidate.api {
                    rpi_ai::Api::AnthropicMessages => anth.is_some(),
                    rpi_ai::Api::OpenaiCompletions | rpi_ai::Api::OpenaiResponses => {
                        openai_key.is_some()
                    }
                    _ => false,
                };
            if !authed {
                continue;
            }
            match build_runtime(
                &candidate.provider,
                &candidate.api,
                &full_catalog,
                (anth.as_ref(), openai_key.as_deref()),
                Some(auth_store),
                settings,
                values,
                factories,
            ) {
                Ok((runtime, _, _)) => providers.push(runtime),
                // Metadata-only or host-extension APIs must not prevent an
                // unrelated native provider from resolving. Direct selection
                // still returns the factory's truthful UnsupportedApi error.
                Err(ResolveError::UnsupportedApi { .. }) => {}
                Err(error) => return Err(error),
            }
        }
    }

    let providers = factories::combine_provider_runtimes(providers);
    let provider = providers
        .iter()
        .find(|provider| provider.id() == selected_provider)
        .expect("selected provider is registered")
        .clone();

    // Only credential-free descriptors leave the resolver. The private binding
    // owns the authoritative endpoint and all request headers.
    model = execution
        .descriptors()
        .iter()
        .find(|candidate| candidate.api == model.api && candidate.id == model.id)
        .expect("selected model is bound")
        .clone();
    let ids: std::collections::BTreeSet<_> = full_catalog
        .iter()
        .map(|model| model.provider.clone())
        .chain(
            factories
                .manifests
                .iter()
                .map(|manifest| manifest.id.clone()),
        )
        .collect();
    let mut snapshot_auth: BTreeMap<_, _> = ids
        .into_iter()
        .map(|id| {
            let state = crate::credentials::state(
                &id,
                models_cfg.providers.get(&id),
                auth_store,
                factories
                    .manifests
                    .iter()
                    .find(|manifest| manifest.id == id),
                values,
                refreshable(&id),
                || {
                    full_catalog
                        .iter()
                        .any(|model| model.provider == id && model_has_header_auth(model))
                },
            );
            (id, state)
        })
        .collect();
    if cli_api_key.is_some() {
        snapshot_auth.insert(
            model.provider.clone(),
            crate::credentials::AuthState {
                status: crate::credentials::AuthStatus::Ready,
                source: crate::credentials::CredentialSource::Explicit,
            },
        );
    }
    let mut snapshot_catalog: Vec<_> = full_catalog.iter().map(binding::descriptor).collect();
    snapshot_catalog.sort_by(|a, b| {
        a.provider
            .to_ascii_lowercase()
            .cmp(&b.provider.to_ascii_lowercase())
            .then_with(|| a.id.to_ascii_lowercase().cmp(&b.id.to_ascii_lowercase()))
    });
    let resolved = ResolvedModel {
        provider,
        model,
        thinking_level,
        has_provider_key,
        theme: settings.theme.clone(),
        providers,
        compat_warnings,
        execution,
        snapshot_catalog,
        snapshot_auth,
    };
    debug_assert!(resolved
        .execution
        .descriptors()
        .iter()
        .all(|model| model.headers.is_none()));
    Ok(resolved)
}

/// Build one provider runtime from a pre-computed credential. Shared by
/// [`resolve_with_factories`] (for the selected provider) and the
/// all-provider runtime list built for `/model` hot-switching.
type BoundRuntime = (Arc<dyn Provider>, Arc<ExecutionBinding>, bool);

#[allow(clippy::too_many_arguments)]
fn build_runtime(
    provider_id: &str,
    api: &rpi_ai::Api,
    catalog: &[Model],
    credentials: (Option<&AnthropicCredential>, Option<&str>),
    auth_store: Option<&config::AuthStore>,
    settings: &RuntimeSettings,
    values: &dyn ValueSource,
    factories: &RuntimeFactories,
) -> Result<BoundRuntime, ResolveError> {
    let (anthropic_credential, openai_key) = credentials;
    let anonymous = factories
        .manifests
        .iter()
        .find(|manifest| manifest.id == provider_id)
        .map(|manifest| !manifest.requires_auth)
        .unwrap_or_else(|| {
            crate::catalog::builtin_provider(provider_id)
                .is_some_and(|provider| !provider.requires_auth)
        })
        || crate::cloud_auth::configured(provider_id, values).is_some();
    let provider_has_auth = anthropic_credential.is_some() || openai_key.is_some() || anonymous;
    let models = catalog
        .iter()
        .filter(|model| model.api == *api && model.provider == provider_id)
        .filter(|model| provider_has_auth || model_has_header_auth(model))
        .cloned()
        .collect();
    let auth = match anthropic_credential {
        Some(AnthropicCredential::ProviderKey(key)) => RequestAuth::ApiKey(key.clone()),
        Some(AnthropicCredential::Headers(headers)) => RequestAuth::Headers(headers.clone()),
        None => openai_key.map_or(RequestAuth::None, |key| RequestAuth::ApiKey(key.into())),
    };
    let has_provider_key = matches!(auth, RequestAuth::ApiKey(_)) || anonymous;
    let oauth = auth_store.and_then(|store| factories.oauth_lease(provider_id, store, &auth));
    let (provider, binding) =
        factories.configure_with_oauth(provider_id, api, models, auth, oauth, settings, values)?;
    Ok((provider, binding, has_provider_key))
}

/// Credential-free descriptors for the selected provider. Authentication was
/// already checked before the common factory created its execution binding.
pub fn available_catalog(resolved: &ResolvedModel) -> Vec<Model> {
    let selected_api = &resolved.model.api;
    let selected_provider = &resolved.model.provider;
    let mut seen = std::collections::HashSet::new();
    resolved
        .provider
        .models()
        .iter()
        // A provider snapshot is expected to be homogeneous, but extension
        // and gateway providers can expose a broader catalog. The selector
        // must only offer models the current lane can actually route to.
        .filter(|m| m.api == *selected_api && m.provider == *selected_provider)
        .filter(|m| seen.insert((m.api.clone(), m.provider.clone(), m.id.clone())))
        .cloned()
        .collect()
}

/// Credential-free descriptors for every configured runtime, including packages.
/// Bound credentials and request headers stay inside each execution binding.
pub fn authenticated_catalog(resolved: &ResolvedModel) -> Vec<Model> {
    let mut seen = std::collections::HashSet::new();
    resolved
        .providers
        .iter()
        .flat_map(|provider| provider.models())
        .filter(|m| seen.insert((m.api.clone(), m.provider.clone(), m.id.clone())))
        .cloned()
        .collect()
}

/// Credential-free catalog and compatibility diagnostics without selecting a model.
pub fn catalog_with_diagnostics(
    mut cfg: config::ModelsConfig,
    manifests: &[crate::ProviderDeclaration],
    values: &dyn ValueSource,
) -> (Vec<Model>, Vec<String>) {
    let (models, warnings) = execution_catalog_with_diagnostics(&mut cfg, manifests, values);
    (models.iter().map(binding::descriptor).collect(), warnings)
}
/// Private configuration view for the host's credential status adapter.
pub(crate) fn execution_catalog_with_diagnostics(
    cfg: &mut config::ModelsConfig,
    manifests: &[crate::ProviderDeclaration],
    values: &dyn ValueSource,
) -> (Vec<Model>, Vec<String>) {
    apply_manifest_defaults(cfg, manifests);
    let warnings = config::model_compat_warnings(cfg);
    let mut catalog = build_catalog(cfg, manifests, values);
    catalog.sort_by(|a, b| {
        a.provider
            .to_ascii_lowercase()
            .cmp(&b.provider.to_ascii_lowercase())
            .then_with(|| a.id.to_ascii_lowercase().cmp(&b.id.to_ascii_lowercase()))
    });
    (catalog, warnings)
}

fn apply_manifest_defaults(
    models_cfg: &mut config::ModelsConfig,
    manifests: &[crate::ProviderDeclaration],
) {
    for manifest in manifests {
        // Give package ids their own configuration namespace. User configuration
        // overrides package defaults; no credentials are stored by declarations.
        if !models_cfg.providers.contains_key(&manifest.id) {
            let first = manifest.models.first();
            let config = serde_json::from_value(serde_json::json!({
                "api": first.map(|model| model.api.as_str()),
                "models":[]
            }))
            .expect("provider config defaults");
            models_cfg.providers.insert(manifest.id.clone(), config);
        }
        if let (Some(default), Some(config)) = (
            manifest.models.first(),
            models_cfg.providers.get_mut(&manifest.id),
        ) {
            config
                .api
                .get_or_insert_with(|| default.api.as_str().to_string());
        }
    }
    crate::catalog::apply_defaults(models_cfg);
}

pub(crate) fn builtin_catalog() -> Vec<Model> {
    let mut catalog = anthropic_models();
    catalog.extend(openai_responses_models());
    let google = serde_json::from_value(serde_json::json!({
        "api": "openai-completions",
        "baseUrl": "https://generativelanguage.googleapis.com/v1beta/openai",
        "models": [{
            "id": "gemini-3.8-flash",
            "name": "Gemini 3.8 Flash",
            "reasoning": true,
            "thinkingLevelMap": {
                "off": "low", "minimal": "low", "low": "low",
                "medium": "medium", "high": "high", "xhigh": "high", "max": "high"
            },
            "contextWindow": 1048576,
            "maxTokens": 65536,
            "input": ["text", "image"],
            "compat": { "supportsReasoningEffort": true, "maxTokensField": "max_tokens" }
        }]
    }))
    .expect("valid built-in Gemini model configuration");
    catalog.extend(
        config::provider_to_models("google", &google, &BTreeMap::new())
            .expect("Gemini uses the OpenAI completions protocol"),
    );
    let mut known: std::collections::BTreeSet<_> = catalog
        .iter()
        .map(|model| (model.provider.clone(), model.id.clone()))
        .collect();
    for provider in crate::catalog::builtin_providers() {
        for model in &provider.models {
            if known.insert((model.provider.clone(), model.id.clone())) {
                let mut model = model.clone();
                let overrides = model.headers.take().unwrap_or_default();
                if !provider.headers.is_empty() {
                    model.headers = Some(provider.headers.clone());
                }
                merge_auth_headers(&mut model, &overrides);
                catalog.push(model);
            }
        }
    }
    catalog
}

pub(crate) fn provider_has_auth_headers(
    provider: &str,
    cfg: &config::ModelsConfig,
    manifests: &[crate::ProviderDeclaration],
    values: &dyn ValueSource,
) -> bool {
    let mut selected = config::ModelsConfig::default();
    if let Some(config) = cfg.providers.get(provider) {
        selected.providers.insert(provider.into(), config.clone());
    }
    let manifests: Vec<_> = manifests
        .iter()
        .filter(|manifest| manifest.id == provider)
        .cloned()
        .collect();
    apply_manifest_defaults(&mut selected, &manifests);
    build_catalog(&selected, &manifests, values)
        .iter()
        .any(|model| model.provider == provider && model_has_header_auth(model))
}

fn build_catalog(
    cfg: &config::ModelsConfig,
    manifests: &[crate::ProviderDeclaration],
    values: &dyn ValueSource,
) -> Vec<Model> {
    let mut catalog = builtin_catalog();
    for model in &mut catalog {
        if let Some(provider) = crate::catalog::builtin_provider(&model.provider) {
            if cfg
                .providers
                .get(&model.provider)
                .is_some_and(|config| config.base_url.is_some())
                || provider
                    .base_url_env
                    .as_ref()
                    .and_then(|name| values.env(name))
                    .is_some()
            {
                model.base_url = crate::catalog::configured_base_url(provider, cfg, values);
            }
        }
        if let Some(headers) = cfg
            .providers
            .get(&model.provider)
            .and_then(|config| config.headers.as_ref())
        {
            merge_auth_headers(
                model,
                &crate::values::resolve_headers(headers, None, values),
            );
        }
    }
    for manifest in manifests {
        // Host declarations own the wire routes for their model identities.
        // Replace bundled routes before adding the declaration's API variants.
        let declared: std::collections::HashSet<_> = manifest
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect();
        catalog
            .retain(|model| model.provider != manifest.id || !declared.contains(model.id.as_str()));
        let config = cfg.providers.get(&manifest.id);
        for mut model in manifest.models.clone() {
            if let Some(base) = config.and_then(|config| config.base_url.as_ref()) {
                model.base_url = base.clone();
            }
            if let Some(headers) = config.and_then(|config| config.headers.as_ref()) {
                merge_auth_headers(
                    &mut model,
                    &crate::values::resolve_headers(headers, None, values),
                );
            }
            catalog.push(model);
        }
    }
    merge_user_catalog(&mut catalog, cfg, values);
    let endpoint_defaults = crate::catalog::EndpointDefaults::new(values);
    for model in &mut catalog {
        endpoint_defaults.apply(model, cfg);
    }
    catalog
}

/// Merge `~/.rpi/models.json` providers into the built-in catalog. Models from
/// the same runtime provider replace entries with the same id, including an
/// explicitly changed wire API. Models from different providers stay distinct
/// so `provider/id` continues selecting the intended endpoint.
fn merge_user_catalog(
    catalog: &mut Vec<Model>,
    cfg: &config::ModelsConfig,
    values: &dyn ValueSource,
) {
    for (provider_id, provider_cfg) in &cfg.providers {
        let Some(models) = config::provider_to_models(provider_id, provider_cfg, values) else {
            // Unsupported protocol.
            continue;
        };
        for m in models {
            if let Some(existing) = catalog.iter_mut().find(|candidate| {
                candidate.provider == m.provider && candidate.id.eq_ignore_ascii_case(&m.id)
            }) {
                *existing = m;
            } else {
                catalog.push(m);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AnthropicCredential {
    /// Passed as the selected provider's default `x-api-key`.
    ProviderKey(String),
    /// Folded only onto models owned by the selected provider.
    Headers(BTreeMap<String, String>),
}

type AnthropicCredentials = BTreeMap<String, AnthropicCredential>;
enum OpenAiCredential {
    Key(String),
    Anonymous,
}
type OpenAiCredentials = BTreeMap<String, OpenAiCredential>;

/// Resolve one independent Anthropic-wire credential for each provider id.
/// Stored credentials win over `models.json`; only the built-in `anthropic`
/// identity may fall back to the `ANTHROPIC_*` environment variables.
#[cfg(test)]
fn resolve_anthropic_credentials(
    cfg: &config::ModelsConfig,
    auth_store: &config::AuthStore,
    manifests: &[crate::ProviderDeclaration],
    values: &dyn ValueSource,
) -> AnthropicCredentials {
    resolve_anthropic_credentials_with(cfg, auth_store, manifests, values, &|_| false)
}

fn resolve_anthropic_credentials_with(
    cfg: &config::ModelsConfig,
    auth_store: &config::AuthStore,
    manifests: &[crate::ProviderDeclaration],
    values: &dyn ValueSource,
    refreshable: &dyn Fn(&str) -> bool,
) -> AnthropicCredentials {
    let mut provider_ids = vec![DEFAULT_PROVIDER_ID.to_string()];
    for (provider_id, provider_cfg) in &cfg.providers {
        if (config::provider_is_anthropic_compatible(provider_cfg)
            || crate::catalog::builtin_provider(provider_id).is_some_and(|provider| {
                provider
                    .models
                    .iter()
                    .any(|model| model.api == rpi_ai::Api::AnthropicMessages)
            })
            || manifests
                .iter()
                .filter(|manifest| manifest.id == *provider_id)
                .any(|manifest| {
                    manifest
                        .models
                        .iter()
                        .any(|model| model.api == rpi_ai::Api::AnthropicMessages)
                }))
            && !provider_ids.iter().any(|known| known == provider_id)
        {
            provider_ids.push(provider_id.clone());
        }
    }
    for manifest in manifests {
        if manifest
            .models
            .iter()
            .any(|model| model.api == rpi_ai::Api::AnthropicMessages)
            && !provider_ids.contains(&manifest.id)
        {
            provider_ids.push(manifest.id.clone());
        }
    }

    let mut credentials = BTreeMap::new();
    for provider_id in provider_ids {
        let resolved = crate::credentials::resolve_refreshable(
            &provider_id,
            cfg.providers.get(&provider_id),
            auth_store,
            manifests.iter().find(|manifest| manifest.id == provider_id),
            values,
            refreshable(&provider_id),
        );
        let credential = resolved
            .key
            .map(|key| anthropic_credential_from_key(key, resolved.bearer));
        if let Some(credential) = credential {
            credentials.insert(provider_id, credential);
        }
    }
    credentials
}

fn anthropic_credential_from_key(key: String, auth_header: bool) -> AnthropicCredential {
    if auth_header {
        AnthropicCredential::Headers(BTreeMap::from([(
            "authorization".to_string(),
            format!("Bearer {key}"),
        )]))
    } else {
        AnthropicCredential::ProviderKey(key)
    }
}

fn anthropic_credential_for<'a>(
    credentials: &'a AnthropicCredentials,
    provider_id: &str,
) -> Option<&'a AnthropicCredential> {
    credentials.get(provider_id)
}

fn credential_for_selected_anthropic_provider(
    cli_api_key: Option<&str>,
    credentials: &AnthropicCredentials,
    provider_id: &str,
    auth_header: bool,
) -> Option<AnthropicCredential> {
    cli_api_key
        .filter(|key| !key.is_empty())
        .map(|key| anthropic_credential_from_key(key.to_string(), auth_header))
        .or_else(|| anthropic_credential_for(credentials, provider_id).cloned())
}

fn resolve_openai_credentials_with(
    cfg: &config::ModelsConfig,
    auth_store: &config::AuthStore,
    manifests: &[crate::ProviderDeclaration],
    values: &dyn ValueSource,
    refreshable: &dyn Fn(&str) -> bool,
) -> OpenAiCredentials {
    let mut provider_ids = vec!["openai".to_string()];
    for provider_id in cfg.providers.keys() {
        if !provider_ids.contains(provider_id) {
            provider_ids.push(provider_id.clone());
        }
    }
    for manifest in manifests {
        if !provider_ids.contains(&manifest.id) {
            provider_ids.push(manifest.id.clone());
        }
    }
    let mut credentials = BTreeMap::new();
    for provider_id in provider_ids {
        let provider_cfg = cfg.providers.get(&provider_id);
        let manifest = manifests.iter().find(|manifest| manifest.id == provider_id);
        let resolved = crate::credentials::resolve_refreshable(
            &provider_id,
            provider_cfg,
            auth_store,
            manifest,
            values,
            refreshable(&provider_id),
        );
        // Anthropic keys are represented by their own map, preserving bearer vs
        // x-api-key semantics. Anonymous declarations apply to every protocol.
        let has_non_anthropic_models =
            crate::catalog::builtin_provider(&provider_id).is_some_and(|provider| {
                provider
                    .models
                    .iter()
                    .any(|model| model.api != rpi_ai::Api::AnthropicMessages)
            }) || manifest.is_some_and(|manifest| {
                manifest
                    .models
                    .iter()
                    .any(|model| model.api != rpi_ai::Api::AnthropicMessages)
            });
        if provider_cfg.is_some_and(config::provider_is_anthropic_compatible)
            && !has_non_anthropic_models
        {
            if matches!(
                resolved.source,
                crate::credentials::CredentialSource::Anonymous
            ) {
                credentials.insert(provider_id, OpenAiCredential::Anonymous);
            }
        } else if let Some(key) = resolved.key {
            credentials.insert(provider_id, OpenAiCredential::Key(key));
        } else if matches!(
            resolved.source,
            crate::credentials::CredentialSource::Anonymous
        ) || crate::cloud_auth::configured(&provider_id, values).is_some()
        {
            credentials.insert(provider_id, OpenAiCredential::Anonymous);
        }
    }
    credentials
}

fn openai_credential_for<'a>(
    credentials: &'a OpenAiCredentials,
    provider_id: &str,
) -> Option<&'a String> {
    match credentials.get(provider_id) {
        Some(OpenAiCredential::Key(key)) => Some(key),
        _ => None,
    }
}

fn merge_auth_headers(model: &mut Model, auth_headers: &BTreeMap<String, String>) {
    let headers = model.headers.get_or_insert_with(BTreeMap::new);
    for (name, value) in auth_headers {
        headers.retain(|existing, _| !existing.eq_ignore_ascii_case(name));
        headers.insert(name.clone(), value.clone());
    }
}

struct SplitModelPattern {
    provider: Option<String>,
    model_id: String,
    raw_model_id: String,
    thinking: Option<ThinkingLevel>,
    inferred_provider: bool,
}

/// Split a `--model` value into its provider, id, and thinking components.
///
/// A slash prefix is provider syntax only when it names a known provider (or
/// matches an explicit `--provider`). Unknown prefixes stay in the raw model
/// id, which is required for OpenRouter-style ids such as
/// `meta-llama/llama-3.3`.
///
/// Mirrors the TS `parseModelPattern` last-colon split + recurse-on-prefix.
fn split_model_pattern(
    value: &str,
    cli_provider: Option<&str>,
    catalog: &[Model],
    cfg: &config::ModelsConfig,
) -> Result<SplitModelPattern, ResolveError> {
    let mut provider = cli_provider.map(str::to_string);
    let mut model_id = value.to_string();
    let mut inferred_provider = false;

    if let Some((prefix, remainder)) = value.split_once('/') {
        if !prefix.is_empty() && !remainder.is_empty() {
            let matches_explicit = cli_provider
                .and_then(|requested| {
                    canonicalize_cli_provider(prefix, cfg)
                        .ok()
                        .map(|canonical| canonical == requested)
                })
                .unwrap_or(false);
            if matches_explicit {
                model_id = remainder.to_string();
            } else if cli_provider.is_none() {
                match canonicalize_cli_provider(prefix, cfg) {
                    Ok(canonical) => {
                        provider = Some(canonical);
                        model_id = remainder.to_string();
                        inferred_provider = true;
                    }
                    Err(ResolveError::UnknownProvider(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
    }

    // upstream attempts the complete id first. Only peel a valid thinking
    // suffix when that complete id does not exist in the selected scope.
    let has_full_exact_match = catalog.iter().any(|model| {
        model.id.eq_ignore_ascii_case(&model_id)
            && provider
                .as_deref()
                .map_or(true, |requested| provider_matches(model, requested, cfg))
    });
    let thinking = if has_full_exact_match {
        None
    } else if let Some((head, suffix)) = model_id.rsplit_once(':') {
        if let Some(level) = parse_thinking_level(suffix) {
            model_id = head.to_string();
            Some(level)
        } else {
            None
        }
    } else {
        None
    };

    Ok(SplitModelPattern {
        provider,
        model_id,
        raw_model_id: value.to_string(),
        thinking,
        inferred_provider,
    })
}

/// Prefer an exact wire id, with case-insensitive lookup as a typing convenience.
fn find_model(
    pattern: &str,
    provider: Option<&str>,
    catalog: &[Model],
    cfg: &config::ModelsConfig,
) -> Option<Model> {
    let belongs =
        |model: &&Model| provider.is_none_or(|requested| provider_matches(model, requested, cfg));
    catalog
        .iter()
        .filter(belongs)
        .find(|model| model.id == pattern)
        .or_else(|| {
            catalog
                .iter()
                .filter(belongs)
                .find(|model| model.id.eq_ignore_ascii_case(pattern))
        })
        .cloned()
}

/// Resolve an exact CLI model reference without silently choosing the first
/// provider when a bare model id is duplicated. This mirrors upstream's
/// `resolveCliModel`: a sole configured-auth match wins; zero or multiple
/// configured-auth matches require an explicit provider.
fn find_cli_model(
    pattern: &str,
    provider: Option<&str>,
    catalog: &[Model],
    cfg: &config::ModelsConfig,
    anthropic_credentials: &AnthropicCredentials,
    openai_credentials: &OpenAiCredentials,
) -> Result<Option<Model>, ResolveError> {
    let mut exact_matches: Vec<&Model> = catalog
        .iter()
        .filter(|model| {
            model.id.eq_ignore_ascii_case(pattern)
                && provider.map_or(true, |requested| provider_matches(model, requested, cfg))
        })
        .collect();
    if exact_matches.iter().any(|model| model.id == pattern) {
        exact_matches.retain(|model| model.id == pattern);
    }

    match exact_matches.as_slice() {
        [] => Ok(None),
        [model] => Ok(Some((*model).clone())),
        _ => {
            let authenticated: Vec<&Model> = exact_matches
                .iter()
                .copied()
                .filter(|model| {
                    model_is_authed_for_resolution(
                        model,
                        anthropic_credentials,
                        openai_credentials,
                        false,
                    )
                })
                .collect();
            if let [model] = authenticated.as_slice() {
                return Ok(Some((*model).clone()));
            }

            let mut matches = exact_matches
                .iter()
                .map(|model| format!("{}/{}", model.provider, model.id))
                .collect::<Vec<_>>();
            matches.sort();
            let auth_hint = if authenticated.is_empty() {
                "No matching provider is authenticated."
            } else {
                "More than one matching provider is authenticated."
            };
            Err(ResolveError::AmbiguousModel {
                pattern: pattern.to_string(),
                matches: matches.join(", "),
                auth_hint,
            })
        }
    }
}

fn provider_is_known(requested: &str, cfg: &config::ModelsConfig) -> bool {
    cfg.providers.contains_key(requested)
        || requested.eq_ignore_ascii_case("anthropic")
        || requested.eq_ignore_ascii_case("openai")
        || requested.eq_ignore_ascii_case("google")
        || requested.eq_ignore_ascii_case("openai-completions")
        || requested.eq_ignore_ascii_case("openai-responses")
}

/// Canonicalize user-facing `--provider` input without weakening provider
/// identity. Exact configured ids win. A case-insensitive custom match is
/// accepted only when unique; otherwise the user must provide exact casing so
/// credentials and endpoints can never cross between distinct ids.
fn canonicalize_cli_provider(
    requested: &str,
    cfg: &config::ModelsConfig,
) -> Result<String, ResolveError> {
    if cfg.providers.contains_key(requested) {
        return Ok(requested.to_string());
    }

    const BUILTIN_PROVIDER_IDS: &[&str] = &[
        "anthropic",
        "openai",
        "google",
        "openai-completions",
        "openai-responses",
    ];
    if BUILTIN_PROVIDER_IDS.contains(&requested) {
        return Ok(requested.to_string());
    }

    let mut matches = cfg
        .providers
        .keys()
        .filter(|provider| provider.eq_ignore_ascii_case(requested))
        .cloned()
        .collect::<Vec<_>>();
    matches.extend(
        BUILTIN_PROVIDER_IDS
            .iter()
            .filter(|provider| provider.eq_ignore_ascii_case(requested))
            .map(|provider| (*provider).to_string()),
    );
    matches.sort();
    matches.dedup();

    match matches.as_slice() {
        [provider] => return Ok(provider.clone()),
        [] => {}
        _ => {
            return Err(ResolveError::AmbiguousProvider {
                requested: requested.to_string(),
                matches: matches.join(", "),
            });
        }
    }

    Err(ResolveError::UnknownProvider(requested.to_string()))
}

fn provider_matches(model: &Model, requested: &str, cfg: &config::ModelsConfig) -> bool {
    // Configured provider ids are exact identities in upstream. An exact
    // config match takes precedence over the case-insensitive built-in/protocol
    // aliases below, so a custom `Anthropic` remains distinct from `anthropic`.
    if model.provider == requested {
        return true;
    }
    if cfg.providers.contains_key(requested) {
        return false;
    }
    if requested.eq_ignore_ascii_case("anthropic") {
        return model.provider == DEFAULT_PROVIDER_ID;
    }
    if requested.eq_ignore_ascii_case("openai") {
        return model.provider == "openai";
    }
    if requested.eq_ignore_ascii_case("openai-completions") {
        return matches!(model.api, rpi_ai::Api::OpenaiCompletions);
    }
    if requested.eq_ignore_ascii_case("openai-responses") {
        return matches!(model.api, rpi_ai::Api::OpenaiResponses);
    }
    false
}

/// Provider identity matching for upstream's default-model table. Unlike the
/// CLI matcher, this intentionally does not treat `openai` as a protocol alias:
/// a default belonging to OpenAI must not select the same model id from an
/// unrelated OpenAI-compatible gateway.
fn model_belongs_to_default_provider(
    model: &Model,
    requested: &str,
    _cfg: &config::ModelsConfig,
) -> bool {
    model.provider == requested
}

/// Whether a catalog model is "configured-auth" — i.e. the request built for it
/// would pass `assertRequestAuth` and not return "No API key". Mirrors the TS
/// `hasConfiguredAuth(providerId)` filter that `getAvailableSnapshot()` applies
/// (`available = all.filter(m => configuredProviders.has(m.provider))`).
///
/// The selected provider snapshot is identity-homogeneous. A model is runnable
/// when its own headers carry auth or that selected provider has a default key.
fn model_is_authed_for_resolution(
    model: &Model,
    anthropic_credentials: &AnthropicCredentials,
    openai_credentials: &OpenAiCredentials,
    has_cli_key: bool,
) -> bool {
    openai_credentials.contains_key(&model.provider)
        || model_has_header_auth(model)
        || has_cli_key
        || match model.api {
            rpi_ai::Api::AnthropicMessages => {
                anthropic_credential_for(anthropic_credentials, &model.provider).is_some()
            }
            rpi_ai::Api::OpenaiCompletions | rpi_ai::Api::OpenaiResponses => {
                openai_credential_for(openai_credentials, &model.provider).is_some()
            }
            _ => false,
        }
}

/// Same three-name check as rpi-ai's `has_header_auth`, but called from the
/// CLI layer (rpi-ai's `has_header_auth` is private to the provider module, so
/// we mirror it here over the model's `headers` map).
fn model_has_header_auth(m: &Model) -> bool {
    crate::credentials::has_header_auth(m.headers.as_ref())
}

/// Choose the default model when `--model` is absent. Mirrors upstream
/// Upstream `findInitialModel` behavior:
/// known-provider defaults are checked in native declaration order, followed by
/// the first authenticated model in the catalog (`availableModels[0]`). This
/// also keeps a models.json-only gateway from accidentally selecting an
/// unauthenticated built-in model.
///
/// Credential maps are keyed by provider id, so only a credential belonging to
/// the candidate model can make it participate in default selection.
fn pick_default_model(
    catalog: &[Model],
    models_cfg: &config::ModelsConfig,
    anthropic_credentials: &AnthropicCredentials,
    openai_credentials: &OpenAiCredentials,
    has_cli_key: bool,
) -> Model {
    // 1. Core provider defaults, in their declared priority order.
    for (provider, model_id) in DEFAULT_MODELS_PER_PROVIDER {
        if let Some(model) = catalog.iter().find(|model| {
            model.id.eq_ignore_ascii_case(model_id)
                && model_belongs_to_default_provider(model, provider, models_cfg)
                && model_is_authed_for_resolution(
                    model,
                    anthropic_credentials,
                    openai_credentials,
                    has_cli_key,
                )
        }) {
            return model.clone();
        }
    }

    // Explicit model lists retain their declaration order even when a newly
    // bundled provider introduces other models under the same identity.
    for (provider, config) in &models_cfg.providers {
        for declared in &config.models {
            if let Some(model) = catalog.iter().find(|model| {
                model.provider == *provider
                    && model.id.eq_ignore_ascii_case(&declared.id)
                    && model_is_authed_for_resolution(
                        model,
                        anthropic_credentials,
                        openai_credentials,
                        has_cli_key,
                    )
            }) {
                return model.clone();
            }
        }
    }

    // 2. First authed model (TS `availableModels[0]`). Provider and model array
    //    declaration order is preserved while loading models.json.
    //    this is the gateway model (Bearer folded onto it, base_url = gateway).
    if let Some(m) = catalog.iter().find(|m| {
        model_is_authed_for_resolution(m, anthropic_credentials, openai_credentials, has_cli_key)
    }) {
        return m.clone();
    }
    // 3. Last resort: the built-in Anthropic default, authed or not. The auth gate above
    //    already errored when no source resolved, so reaching here means *some*
    //    auth exists but none folded/attached to a model we can see — keep the
    //    historical default to avoid a NoMatch surprise.
    catalog
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(DEFAULT_MODEL_ID))
        .or_else(|| catalog.first())
        .expect("catalog is never empty (built-in anthropic_models)")
        .clone()
}

#[cfg(test)]
mod tests;
