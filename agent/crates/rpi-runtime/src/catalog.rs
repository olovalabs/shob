//! Native, frozen provider/model metadata and credential-scoped HTTP discovery.
//!
//! Data is derived from oh-my-pi's MIT-licensed catalog at [`UPSTREAM_REVISION`],
//! unioned with Kathamo's pre-existing provider catalog. No upstream code executes.
use std::{collections::BTreeMap, sync::LazyLock, time::Duration};

use rpi_ai::{Api, InputModality, Model, ModelCost, StreamingProtocolCompat};
use serde::Deserialize;

use crate::{
    config::{AuthStore, ModelsConfig},
    ProviderDeclaration, ValueSource,
};

pub const UPSTREAM_REVISION: &str = "73d7521269a1212fc2b2790a2e8c08424496c953";
pub const UPSTREAM_SOURCE: &str = "https://github.com/can1357/oh-my-pi";

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuiltinProvider {
    pub id: String,
    pub name: String,
    pub env_vars: Vec<String>,
    pub api: Api,
    pub base_url: String,
    pub base_url_env: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub requires_auth: bool,
    pub discovery: bool,
    /// Public roster discovery does not authorize inference requests.
    #[serde(default)]
    pub discovery_allows_unauthenticated: bool,
    pub default_model: Option<String>,
    pub models: Vec<Model>,
    /// All bundled model kinds and extra upstream capabilities, including
    /// non-chat task models that must not be routed through the chat picker.
    pub model_metadata: BTreeMap<String, BuiltinModelMetadata>,
}

#[derive(Clone, Deserialize)]
pub struct BuiltinModelMetadata {
    pub kind: String,
    #[serde(flatten)]
    pub capabilities: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct CatalogData {
    providers: Vec<BuiltinProvider>,
}

static PROVIDERS: LazyLock<Vec<BuiltinProvider>> = LazyLock::new(|| {
    serde_json::from_str::<CatalogData>(include_str!("catalog.json"))
        .expect("frozen native provider catalog must match its Rust schema")
        .providers
});

pub fn builtin_providers() -> &'static [BuiltinProvider] {
    &PROVIDERS
}

pub fn builtin_provider(id: &str) -> Option<&'static BuiltinProvider> {
    PROVIDERS.iter().find(|provider| provider.id == id)
}

pub fn model_metadata(provider: &str, model: &str) -> Option<&'static BuiltinModelMetadata> {
    builtin_provider(provider)?.model_metadata.get(model)
}

/// Unknown custom chat protocols remain selectable, but known task APIs never
/// become chat merely because a dynamically discovered id lacks static metadata.
pub fn is_chat_model(model: &Model) -> bool {
    if matches!(
        model.api.as_str(),
        "openai-embeddings"
            | "openai-images"
            | "openai-transcriptions"
            | "openai-speech"
            | "openai-decisions"
            | "openai-rerank"
            | "openai-video"
            | "openrouter-images"
            | "openrouter-video"
            | "openrouter-rerank"
            | "openrouter-decisions"
            | "xai-tts"
            | "web-search"
            | "local-inference"
            | "typesafe"
            | "workers-ai-systemone"
    ) {
        return false;
    }
    model_metadata(&model.provider, &model.id).is_none_or(|metadata| metadata.kind == "chat")
}

/// Register identities without inventing models for dynamic-only providers.
/// Do not fill baseUrl here: providers can legitimately mix multiple endpoints.
pub(crate) fn apply_defaults(config: &mut ModelsConfig) {
    for provider in builtin_providers() {
        let entry = config
            .providers
            .entry(provider.id.clone())
            .or_insert_with(|| {
                serde_json::from_value(serde_json::json!({"models": []}))
                    .expect("empty provider configuration")
            });
        entry
            .api
            .get_or_insert_with(|| provider.api.as_str().to_owned());
    }
}

pub(crate) fn configured_base_url(
    provider: &BuiltinProvider,
    config: &ModelsConfig,
    values: &dyn ValueSource,
) -> String {
    config
        .providers
        .get(&provider.id)
        .and_then(|cfg| cfg.base_url.clone())
        .or_else(|| {
            provider
                .base_url_env
                .as_ref()
                .and_then(|name| values.env(name))
        })
        .filter(|base| !base.trim().is_empty())
        .unwrap_or_else(|| provider.base_url.clone())
}

/// Cloud environment inputs are read once per catalog, including shared AWS
/// profile files, rather than once for every model on the same provider.
pub(crate) struct EndpointDefaults {
    azure_base: Option<String>,
    azure_version: Option<String>,
    azure_deployments: BTreeMap<String, String>,
    azure_deployment: Option<String>,
    vertex_project: Option<String>,
    vertex_location: String,
    aws_region: Option<String>,
}

impl EndpointDefaults {
    pub(crate) fn new(values: &dyn ValueSource) -> Self {
        let env = |name: &str| values.env(name).filter(|value| !value.trim().is_empty());
        let azure_base = env("AZURE_OPENAI_BASE_URL")
            .or_else(|| {
                env("AZURE_OPENAI_ENDPOINT").map(|endpoint| {
                    let mut endpoint = endpoint.trim_end_matches('/').to_owned();
                    if reqwest::Url::parse(&endpoint)
                        .is_ok_and(|url| url.path().trim_matches('/').is_empty())
                    {
                        endpoint.push_str("/openai/v1");
                    }
                    endpoint
                })
            })
            .or_else(|| {
                env("AZURE_OPENAI_RESOURCE_NAME")
                    .map(|resource| format!("https://{resource}.openai.azure.com/openai/v1"))
            });
        let azure_deployments = env("AZURE_OPENAI_DEPLOYMENT_NAME_MAP")
            .map(|mapping| {
                mapping
                    .split(',')
                    .filter_map(|entry| entry.trim().split_once('='))
                    .filter(|(id, deployment)| {
                        !id.trim().is_empty() && !deployment.trim().is_empty()
                    })
                    .map(|(id, deployment)| (id.trim().to_owned(), deployment.trim().to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            azure_base,
            azure_deployments,
            azure_version: env("AZURE_OPENAI_API_VERSION"),
            azure_deployment: env("AZURE_OPENAI_DEPLOYMENT_NAME"),
            vertex_project: env("GOOGLE_CLOUD_PROJECT")
                .or_else(|| env("GOOGLE_CLOUD_PROJECT_ID"))
                .or_else(|| env("GCLOUD_PROJECT")),
            vertex_location: env("GOOGLE_CLOUD_LOCATION").unwrap_or_else(|| "global".to_owned()),
            aws_region: crate::cloud_auth::aws_region(values),
        }
    }

    /// User endpoints and headers remain authoritative. Missing cloud resource
    /// values are resolved by the native request-time credential/endpoint path.
    pub(crate) fn apply(&self, model: &mut Model, config: &ModelsConfig) {
        let explicit_base = config.providers.get(&model.provider).is_some_and(|config| {
            config.base_url.is_some()
                || config.models.iter().any(|configured| {
                    configured.id.eq_ignore_ascii_case(&model.id) && configured.base_url.is_some()
                })
        });
        let mut headers = BTreeMap::new();
        if model.provider == "azure" {
            if !explicit_base {
                if let Some(base) = &self.azure_base {
                    model.base_url = base.clone();
                }
            }
            if let Some(version) = &self.azure_version {
                headers.insert("x-omp-azure-api-version".to_owned(), version.clone());
            }
            if let Some(deployment) = self
                .azure_deployments
                .get(&model.id)
                .or(self.azure_deployment.as_ref())
            {
                headers.insert("x-omp-azure-deployment".to_owned(), deployment.clone());
            }
        }
        if model.provider == "google-vertex" {
            let location = &self.vertex_location;
            headers.insert("x-goog-location".to_owned(), location.clone());
            if let Some(project) = &self.vertex_project {
                headers.insert("x-omp-google-project".to_owned(), project.clone());
            }
            if !explicit_base {
                if model.api == Api::GoogleVertex {
                    model.base_url = if location == "global" {
                        "https://aiplatform.googleapis.com".to_owned()
                    } else {
                        format!("https://{location}-aiplatform.googleapis.com")
                    };
                } else {
                    model.base_url = model.base_url.replace("{location}", location).replace(
                        "global-aiplatform.googleapis.com",
                        "aiplatform.googleapis.com",
                    );
                    if let Some(project) = &self.vertex_project {
                        model.base_url = model.base_url.replace("{project}", project);
                    }
                }
            }
        }
        if matches!(model.provider.as_str(), "amazon-bedrock" | "bedrock-mantle") {
            if let Some(region) = &self.aws_region {
                headers.insert("x-omp-aws-region".to_owned(), region.clone());
                if !explicit_base {
                    model.base_url = model.base_url.replace("{region}", region);
                }
            }
        }
        if !headers.is_empty() {
            for (name, value) in model.headers.take().unwrap_or_default() {
                headers.retain(|existing, _| !existing.eq_ignore_ascii_case(&name));
                headers.insert(name, value);
            }
            model.headers = Some(headers);
        }
    }
}

/// Discover the real server roster, never a fabricated default model id.
/// Local servers and the public Hyper catalog can be probed without a key;
/// credential-scoped remote providers are probed only when a key is available.
pub async fn discover_models(
    config: &ModelsConfig,
    credentials: &AuthStore,
    values: &dyn ValueSource,
) -> (Vec<ProviderDeclaration>, Vec<String>) {
    discover_models_for(config, credentials, values, None).await
}

/// Startup resolution probes only the selected provider. Catalog browsing uses
/// `None` and concurrently probes the available discovery endpoints.
pub async fn discover_models_for(
    config: &ModelsConfig,
    credentials: &AuthStore,
    values: &dyn ValueSource,
    selected: Option<&str>,
) -> (Vec<ProviderDeclaration>, Vec<String>) {
    let mut jobs = tokio::task::JoinSet::new();
    let mut warnings = Vec::new();
    for provider in builtin_providers().iter().filter(|provider| {
        provider.discovery && selected.is_none_or(|id| id.eq_ignore_ascii_case(&provider.id))
    }) {
        let configured = config.providers.get(&provider.id);
        let key =
            crate::credentials::resolve(&provider.id, configured, credentials, None, values).key;
        let base = configured_base_url(provider, config, values);
        let mut headers = provider.headers.clone();
        if let Some(configured) = configured.and_then(|cfg| cfg.headers.as_ref()) {
            headers.extend(crate::values::resolve_headers(configured, None, values));
        }
        if !provider.discovery_allows_unauthenticated
            && key.is_none()
            && !crate::credentials::has_header_auth(Some(&headers))
        {
            continue;
        }
        let api = configured
            .and_then(|cfg| cfg.api.as_ref())
            .and_then(|api| serde_json::from_value(serde_json::Value::String(api.clone())).ok())
            .unwrap_or_else(|| provider.api.clone());
        match crate::provider::provider_http_client(
            &[],
            &base,
            &crate::RuntimeSettings::default(),
            values,
        ) {
            Ok(client) => {
                jobs.spawn(discover_provider(
                    provider,
                    base,
                    api,
                    headers,
                    key,
                    client,
                    configured.is_some_and(|config| config.api.is_some()),
                ));
            }
            Err(error) => warnings.push(format!("{} model discovery: {error}", provider.id)),
        }
    }
    let mut declarations = Vec::new();
    while let Some(result) = jobs.join_next().await {
        match result {
            Ok(Ok(declaration)) => declarations.push(declaration),
            Ok(Err(warning)) => warnings.push(warning),
            Err(error) => warnings.push(format!("Model discovery task failed: {error}")),
        }
    }
    declarations.sort_by(|a, b| a.id.cmp(&b.id));
    (declarations, warnings)
}

async fn discover_provider(
    provider: &'static BuiltinProvider,
    base: String,
    api: Api,
    headers: BTreeMap<String, String>,
    key: Option<String>,
    client: reqwest::Client,
    explicit_api: bool,
) -> Result<ProviderDeclaration, String> {
    let mut endpoint = reqwest::Url::parse(base.trim())
        .map_err(|_| format!("{} model discovery has an invalid endpoint", provider.id))?;
    let mut path = endpoint.path().trim_end_matches('/').to_owned();
    if matches!(provider.id.as_str(), "ollama" | "charm-hyper") && !path.ends_with("/v1") {
        path.push_str("/v1");
    }
    endpoint.set_path(&path);
    let base = endpoint.as_str().to_owned();
    let mut roster_url = endpoint.clone();
    roster_url.set_path(&format!("{path}/models"));
    let timeout = if matches!(endpoint.host_str(), Some("127.0.0.1" | "localhost" | "::1")) {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(10)
    };
    let mut request = client.get(roster_url).timeout(timeout);
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    if let Some(key) = key.as_ref() {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| format!("{} model discovery: {}", provider.id, error.without_url()))?;
    let payload: serde_json::Value = response.json().await.map_err(|error| {
        format!(
            "{} model discovery JSON: {}",
            provider.id,
            error.without_url()
        )
    })?;
    let rows = payload
        .get("data")
        .and_then(serde_json::Value::as_array)
        .or_else(|| payload.get("models").and_then(serde_json::Value::as_array))
        .or_else(|| payload.as_array())
        .ok_or_else(|| format!("{} model discovery returned no model array", provider.id))?;
    let rich = if provider.id == "litellm" {
        litellm_metadata(&client, &endpoint, &headers, key.as_deref(), timeout).await
    } else {
        BTreeMap::new()
    };
    let mut models = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(id) = row
            .get("id")
            .or_else(|| row.get("name"))
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
        else {
            continue;
        };
        if let Some(output) = row
            .pointer("/architecture/output_modalities")
            .and_then(serde_json::Value::as_array)
        {
            if !output.is_empty() && !output.iter().any(|value| value == "text") {
                continue;
            }
        }
        if matches!(
            row.get("type").and_then(serde_json::Value::as_str),
            Some("embedding" | "image" | "audio")
        ) {
            continue;
        }
        let mut model = provider
            .models
            .iter()
            .find(|model| model.id == id)
            .cloned()
            .unwrap_or_else(|| Model::new(id, id, api.clone(), &provider.id, &base));
        model.api = api.clone();
        if provider.id == "litellm" && !explicit_api {
            model.api = litellm_api(rich.get(id).unwrap_or(row), id);
        }
        model.base_url = base.clone();
        model.name = row
            .get("display_name")
            .or_else(|| row.get("name"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(id)
            .to_owned();
        model.context_window = positive_number(
            row,
            &[
                "context_window",
                "context_length",
                "max_model_len",
                "max_input_tokens",
            ],
        )
        .unwrap_or(model.context_window);
        model.max_tokens = positive_number(
            row,
            &["max_output_tokens", "max_tokens", "max_completion_tokens"],
        )
        .unwrap_or(model.max_tokens);
        if let Some(metadata) = rich.get(id) {
            let info = metadata.get("model_info").unwrap_or(metadata);
            model.context_window = positive_number(info, &["max_input_tokens", "max_tokens"])
                .unwrap_or(model.context_window);
            model.max_tokens =
                positive_number(info, &["max_output_tokens"]).unwrap_or(model.max_tokens);
            model.reasoning |= info
                .get("supports_reasoning")
                .and_then(serde_json::Value::as_bool)
                == Some(true);
            if info
                .get("supports_vision")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
            {
                model.input = vec![InputModality::Text, InputModality::Image];
            }
            if info.get("input_cost_per_token").is_some()
                || info.get("output_cost_per_token").is_some()
            {
                model.cost.rates = rpi_ai::ModelCostRates {
                    input: number(info, &["input_cost_per_token"]) * 1_000_000.0,
                    output: number(info, &["output_cost_per_token"]) * 1_000_000.0,
                    cache_read: number(info, &["cache_read_input_token_cost"]) * 1_000_000.0,
                    cache_write: number(info, &["cache_creation_input_token_cost"]) * 1_000_000.0,
                };
                model.cost_configured = true;
            }
        }
        if provider.id == "ollama" {
            let native_path = path.strip_suffix("/v1").unwrap_or(&path);
            let mut show_url = endpoint.clone();
            show_url.set_path(&format!("{native_path}/api/show"));
            let mut show = client
                .post(show_url)
                .timeout(timeout)
                .json(&serde_json::json!({"model": id}));
            if let Some(key) = key.as_ref() {
                show = show.bearer_auth(key);
            }
            if let Ok(response) = show
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
            {
                if let Ok(metadata) = response.json::<serde_json::Value>().await {
                    if let Some(info) = metadata
                        .get("model_info")
                        .and_then(serde_json::Value::as_object)
                    {
                        if let Some(context) = info
                            .iter()
                            .filter(|(name, _)| name.ends_with(".context_length"))
                            .filter_map(|(_, value)| value.as_u64())
                            .max()
                        {
                            model.context_window = context;
                        }
                    }
                    if let Some(capabilities) = metadata
                        .get("capabilities")
                        .and_then(serde_json::Value::as_array)
                    {
                        model.reasoning |= capabilities.iter().any(|value| value == "thinking");
                        if capabilities.iter().any(|value| value == "vision") {
                            model.input = vec![InputModality::Text, InputModality::Image];
                        }
                    }
                }
            }
        }
        let vision = row
            .pointer("/capabilities/vision")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
            || row
                .pointer("/architecture/input_modalities")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|values| values.iter().any(|value| value == "image"));
        if vision {
            model.input = vec![InputModality::Text, InputModality::Image];
        }
        let efforts: Vec<_> = row
            .pointer("/reasoning/effort_levels")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|level| level.get("value").and_then(serde_json::Value::as_str))
            .collect();
        let reasoning = efforts.iter().any(|effort| *effort != "none")
            || row.get("reasoning").and_then(serde_json::Value::as_bool) == Some(true)
            || row
                .get("supported_parameters")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|values| values.iter().any(|value| value == "reasoning"));
        model.reasoning |= reasoning;
        if !efforts.is_empty() {
            let mut map = BTreeMap::new();
            for level in [
                rpi_ai::ThinkingLevel::Off,
                rpi_ai::ThinkingLevel::Minimal,
                rpi_ai::ThinkingLevel::Low,
                rpi_ai::ThinkingLevel::Medium,
                rpi_ai::ThinkingLevel::High,
                rpi_ai::ThinkingLevel::Xhigh,
                rpi_ai::ThinkingLevel::Max,
            ] {
                map.insert(level, None);
            }
            for effort in efforts {
                if effort == "none" {
                    map.insert(rpi_ai::ThinkingLevel::Off, Some("none".to_owned()));
                    continue;
                }
                if let Ok(level) =
                    serde_json::from_value(serde_json::Value::String(effort.to_owned()))
                {
                    map.insert(level, Some(effort.to_owned()));
                }
            }
            model.thinking_level_map = Some(map);
        }
        if let Some(pricing) = row.get("pricing") {
            model.cost = ModelCost {
                rates: rpi_ai::ModelCostRates {
                    input: number(pricing, &["input_price_per_million", "input"]),
                    output: number(pricing, &["output_price_per_million", "output"]),
                    cache_read: number(pricing, &["cache_read_price_per_million", "cache_hit"]),
                    cache_write: number(
                        pricing,
                        &["cache_write_price_per_million", "cache_create"],
                    ),
                },
                ..Default::default()
            };
            model.cost_configured = true;
        }
        if provider.id == "otari" {
            model.compat = Some(StreamingProtocolCompat::OpenaiCompletions(
                rpi_ai::model::OpenaiCompletionsCompat {
                    supports_developer_role: Some(false),
                    ..Default::default()
                },
            ));
        }
        if !headers.is_empty() {
            model.headers = Some(headers.clone());
        }
        models.push(model);
    }
    Ok(ProviderDeclaration {
        id: provider.id.clone(),
        models,
        api_key_env: provider.env_vars.first().cloned(),
        requires_auth: provider.requires_auth,
    })
}

fn number(row: &serde_json::Value, names: &[&str]) -> f64 {
    names
        .iter()
        .filter_map(|name| row.get(name))
        .find_map(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
        .filter(|number| number.is_finite() && *number >= 0.0)
        .unwrap_or(0.0)
}

fn positive_number(row: &serde_json::Value, names: &[&str]) -> Option<u64> {
    let value = number(row, names);
    (value > 0.0).then_some(value as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_readme_integrations_have_native_identities() {
        let requested = [
            "anthropic",
            "openai",
            "openai-codex",
            "google",
            "google-vertex",
            "google-antigravity",
            "xai",
            "xai-oauth",
            "deepseek",
            "mistral",
            "groq",
            "cerebras",
            "sambanova",
            "fireworks",
            "together",
            "modular",
            "baseten",
            "deepinfra",
            "huggingface",
            "nvidia",
            "meta",
            "inception",
            "amazon-bedrock",
            "azure",
            "siliconflow",
            "gmi-cloud",
            "coreweave",
            "sakana",
            "commandcode",
            "charm-hyper",
            "stepfun",
            "helmcode",
            "openrouter",
            "synthetic",
            "vercel-ai-gateway",
            "cloudflare-ai-gateway",
            "wafer-serverless",
            "scaleway",
            "otari",
            "cursor",
            "github-copilot",
            "gitlab-duo",
            "devin",
            "kimi-code",
            "moonshot",
            "minimax-code",
            "minimax-code-cn",
            "alibaba-coding-plan",
            "qwen-portal",
            "zai",
            "zhipu-coding-plan",
            "xiaomi",
            "qianfan",
            "umans",
            "nanogpt",
            "novita",
            "venice",
            "kilo",
            "zenmux",
            "opencode-go",
            "opencode-zen",
            "ollama",
            "ollama-cloud",
            "lm-studio",
            "llama.cpp",
            "vllm",
            "litellm",
        ];
        for id in requested {
            let provider = builtin_provider(id).unwrap_or_else(|| panic!("missing {id}"));
            assert_ne!(
                provider.api.as_str(),
                "unavailable",
                "{id} needs a real wire API"
            );
            assert!(
                !provider.base_url.is_empty() || id == "azure",
                "{id} needs an endpoint source"
            );
            assert!(
                provider.discovery || !provider.models.is_empty(),
                "{id} has no model source"
            );
        }
    }

    #[test]
    fn dynamic_only_integrations_do_not_invent_models() {
        for id in [
            "ollama",
            "lm-studio",
            "llama.cpp",
            "vllm",
            "litellm",
            "otari",
            "charm-hyper",
        ] {
            let provider = builtin_provider(id).unwrap();
            assert!(provider.discovery);
            assert!(
                provider.models.is_empty(),
                "{id} must use the actual server roster"
            );
        }
    }

    #[test]
    fn existing_runtime_models_and_user_endpoint_overrides_are_preserved() {
        let config = serde_json::from_value(serde_json::json!({
            "providers": { "google": {
                "baseUrl": "https://example.test/custom", "models": []
            }}
        }))
        .unwrap();
        let (models, _) = crate::provider::catalog_with_diagnostics(config, &[], &BTreeMap::new());
        assert!(models
            .iter()
            .any(|model| model.provider == "anthropic"
                && model.id == crate::provider::DEFAULT_MODEL_ID));
        assert!(models
            .iter()
            .any(|model| model.provider == "google" && model.id == "gemini-3.8-flash"));
        assert!(models
            .iter()
            .filter(|model| model.provider == "google")
            .all(|model| model.base_url == "https://example.test/custom"));
        assert!(models
            .iter()
            .any(|model| model.provider == "xai-oauth" && model.id == "grok-4.7"));
    }

    #[test]
    fn nonchat_models_retain_metadata_without_claiming_chat_support() {
        let provider = builtin_provider("openrouter").unwrap();
        let model = provider
            .models
            .iter()
            .find(|model| {
                model_metadata(&model.provider, &model.id)
                    .is_some_and(|metadata| metadata.kind == "image")
            })
            .expect("upstream OpenRouter image models remain discoverable");
        assert!(!is_chat_model(model));
        assert!(provider.models.iter().any(is_chat_model));
        for api in [
            "openai-decisions",
            "openai-embeddings",
            "openrouter-video",
            "xai-tts",
        ] {
            let model = Model::new(
                "server-discovered-id",
                "Dynamic task",
                Api::Other(api.to_owned()),
                "litellm",
                "http://localhost:4000/v1",
            );
            assert!(model_metadata(&model.provider, &model.id).is_none());
            assert!(!is_chat_model(&model), "{api} is not a chat transport");
        }
        let custom = Model::new(
            "custom-id",
            "Custom chat",
            Api::Other("custom-chat".to_owned()),
            "custom",
            "https://custom.test/v1",
        );
        assert!(is_chat_model(&custom));
    }

    #[tokio::test]
    async fn discovery_uses_server_ids_credentials_and_capabilities() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let mut bytes = [0; 1024];
                let count = socket.read(&mut bytes).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&bytes[..count]);
            }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            assert!(request.starts_with("get /api/v1/models "));
            assert!(request.contains("authorization: bearer fixture-workspace-key"));
            assert!(request.contains("x-workspace: fixture"));
            let body = r#"{"data":[{"id":"workspace/actual-model","name":"Workspace model","context_window":262144,"max_output_tokens":16384,"supported_parameters":["reasoning"],"architecture":{"input_modalities":["text","image"],"output_modalities":["text"]},"pricing":{"input_price_per_million":2,"output_price_per_million":6}}]}"#;
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        let config: ModelsConfig = serde_json::from_value(serde_json::json!({
            "providers": {"otari": {
                "baseUrl": format!("http://{address}/api/v1"),
                "headers": {"X-Workspace":"fixture"}, "models": []
            }}
        }))
        .unwrap();
        let credentials = AuthStore::from([(
            "otari".to_owned(),
            crate::config::Credential::ApiKey {
                key: Some("fixture-workspace-key".to_owned()),
                env: None,
            },
        )]);
        let (declarations, warnings) =
            discover_models_for(&config, &credentials, &BTreeMap::new(), Some("otari")).await;
        server.join().unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let models = &declarations[0].models;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "workspace/actual-model");
        assert_eq!(models[0].context_window, 262144);
        assert_eq!(models[0].max_tokens, 16384);
        assert!(models[0].reasoning);
        assert_eq!(
            models[0].input,
            vec![InputModality::Text, InputModality::Image]
        );
        assert_eq!(models[0].cost.rates.input, 2.0);
    }

    #[test]
    fn unrelated_metadata_only_protocols_do_not_break_native_resolution() {
        let values = BTreeMap::from([("OPENAI_API_KEY".to_owned(), "fixture-key".to_owned())]);
        let resolved = crate::RuntimeConfig::default()
            .resolve(
                &crate::ModelRequest {
                    provider: Some("openai"),
                    model: Some("gpt-6-astra"),
                    ..Default::default()
                },
                &crate::ProviderRegistry::default(),
                &values,
            )
            .unwrap();
        assert_eq!(resolved.model.id, "gpt-6-astra");
    }

    #[test]
    fn public_model_rosters_do_not_authenticate_inference() {
        let registry = crate::ProviderRegistry::default();
        let values = BTreeMap::new();
        let mut config = crate::RuntimeConfig::default();
        for provider in ["openrouter", "charm-hyper", "commandcode", "zenmux"] {
            assert!(!config.auth_status(provider, &registry, &values).is_ready());
        }
        config.credentials.insert(
            "openrouter".into(),
            crate::config::Credential::ApiKey {
                key: Some("fixture-key".into()),
                env: None,
            },
        );
        assert!(config
            .auth_status("openrouter", &registry, &values)
            .is_ready());
        assert!(!config
            .auth_status("charm-hyper", &registry, &values)
            .is_ready());
    }

    #[test]
    fn mixed_protocols_share_only_their_own_provider_credentials() {
        let mut config = crate::RuntimeConfig::default();
        config.credentials.insert(
            "github-copilot".to_owned(),
            crate::config::Credential::ApiKey {
                key: Some("copilot-fixture".to_owned()),
                env: None,
            },
        );
        let model = builtin_provider("github-copilot")
            .unwrap()
            .models
            .iter()
            .find(|model| model.api == Api::AnthropicMessages && is_chat_model(model))
            .unwrap();
        let resolved = config
            .resolve(
                &crate::ModelRequest {
                    provider: Some("github-copilot"),
                    model: Some(&model.id),
                    ..Default::default()
                },
                &crate::ProviderRegistry::default(),
                &BTreeMap::new(),
            )
            .unwrap();
        assert_eq!(resolved.model.api, Api::AnthropicMessages);
        for provider in ["commandcode", "zenmux"] {
            let mut config = crate::RuntimeConfig::default();
            config.credentials.insert(
                provider.to_owned(),
                crate::config::Credential::ApiKey {
                    key: Some(format!("{provider}-fixture")),
                    env: None,
                },
            );
            let model = builtin_provider(provider)
                .unwrap()
                .models
                .iter()
                .find(|model| model.api != Api::AnthropicMessages && is_chat_model(model))
                .unwrap();
            let resolved = config
                .resolve(
                    &crate::ModelRequest {
                        provider: Some(provider),
                        model: Some(&model.id),
                        ..Default::default()
                    },
                    &crate::ProviderRegistry::default(),
                    &BTreeMap::new(),
                )
                .unwrap();
            assert_eq!(resolved.model.provider, provider);
        }
    }

    #[test]
    fn kimi_materializes_its_real_anthropic_protocol_and_client_headers() {
        let models = crate::provider::builtin_catalog();
        for model in models.iter().filter(|model| model.provider == "kimi-code") {
            assert_eq!(model.api, Api::AnthropicMessages);
            let headers = model.headers.as_ref().unwrap();
            assert_eq!(headers["User-Agent"], "KimiCLI/1.0");
            assert_eq!(headers["X-Msh-Platform"], "kimi_cli");
        }
    }

    #[test]
    fn cloud_environment_defaults_preserve_explicit_model_overrides() {
        let values = BTreeMap::from([
            (
                "AZURE_OPENAI_RESOURCE_NAME".to_owned(),
                "fixture-resource".to_owned(),
            ),
            (
                "AZURE_OPENAI_API_VERSION".to_owned(),
                "2025-04-01-preview".to_owned(),
            ),
            (
                "AZURE_OPENAI_DEPLOYMENT_NAME_MAP".to_owned(),
                "gpt-fixture=workspace-deployment".to_owned(),
            ),
            ("GOOGLE_CLOUD_LOCATION".to_owned(), "us-central1".to_owned()),
            (
                "GOOGLE_CLOUD_PROJECT".to_owned(),
                "fixture-project".to_owned(),
            ),
            ("AWS_REGION".to_owned(), "eu-west-1".to_owned()),
        ]);
        let defaults = EndpointDefaults::new(&values);
        let mut azure = Model::new("gpt-fixture", "GPT", Api::AzureOpenaiResponses, "azure", "");
        defaults.apply(&mut azure, &ModelsConfig::default());
        assert_eq!(
            azure.base_url,
            "https://fixture-resource.openai.azure.com/openai/v1"
        );
        assert_eq!(
            azure.headers.as_ref().unwrap()["x-omp-azure-deployment"],
            "workspace-deployment"
        );
        let explicit = serde_json::from_value(serde_json::json!({
            "providers":{"azure":{"models":[{"id":"gpt-fixture","baseUrl":"https://custom.test/api"}]}}
        })).unwrap();
        azure.base_url = "https://custom.test/api".to_owned();
        defaults.apply(&mut azure, &explicit);
        assert_eq!(azure.base_url, "https://custom.test/api");
        let mut vertex = Model::new(
            "gemini-fixture",
            "Gemini",
            Api::GoogleVertex,
            "google-vertex",
            "https://{location}-aiplatform.googleapis.com",
        );
        defaults.apply(&mut vertex, &ModelsConfig::default());
        assert_eq!(
            vertex.base_url,
            "https://us-central1-aiplatform.googleapis.com"
        );
        assert_eq!(
            vertex.headers.as_ref().unwrap()["x-omp-google-project"],
            "fixture-project"
        );
        let mut bedrock = Model::new(
            "claude-fixture",
            "Claude",
            Api::BedrockConverseStream,
            "amazon-bedrock",
            "",
        );
        defaults.apply(&mut bedrock, &ModelsConfig::default());
        assert_eq!(
            bedrock.headers.as_ref().unwrap()["x-omp-aws-region"],
            "eu-west-1"
        );
    }
}

/// Management endpoints enrich only the credential-scoped `/v1/models` roster;
/// their additional ids never become selectable under a narrower virtual key.
async fn litellm_metadata(
    client: &reqwest::Client,
    base: &reqwest::Url,
    headers: &BTreeMap<String, String>,
    key: Option<&str>,
    timeout: Duration,
) -> BTreeMap<String, serde_json::Value> {
    let mut result = BTreeMap::new();
    let root = base
        .path()
        .trim_end_matches('/')
        .strip_suffix("/v1")
        .unwrap_or(base.path().trim_end_matches('/'));
    for route in [
        "/model_group/info",
        "/v2/model/info",
        "/model/info",
        "/v1/model/info",
    ] {
        let mut page = 1;
        loop {
            let mut url = base.clone();
            url.set_path(&format!("{root}{route}"));
            if page > 1 {
                url.query_pairs_mut().append_pair("page", &page.to_string());
            }
            let mut request = client.get(url).timeout(timeout);
            for (name, value) in headers {
                request = request.header(name, value);
            }
            if let Some(key) = key {
                request = request.bearer_auth(key);
            }
            let Ok(response) = request
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
            else {
                break;
            };
            let Ok(payload) = response.json::<serde_json::Value>().await else {
                break;
            };
            let entries = payload.as_array().or_else(|| {
                ["data", "models", "result", "items"]
                    .iter()
                    .find_map(|key| payload.get(key).and_then(serde_json::Value::as_array))
            });
            if let Some(entries) = entries {
                for entry in entries {
                    if let Some(id) = ["model_group", "model_name", "id"]
                        .iter()
                        .find_map(|key| entry.get(key).and_then(serde_json::Value::as_str))
                    {
                        // The group-level API owns routing; later deployment
                        // rows only fill metadata for missing group identities.
                        result.entry(id.to_owned()).or_insert_with(|| entry.clone());
                    }
                }
            }
            let pages = payload
                .get("total_pages")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(1)
                .min(100);
            if route != "/v2/model/info" || page >= pages {
                break;
            }
            page += 1;
        }
    }
    result
}

fn litellm_api(metadata: &serde_json::Value, id: &str) -> Api {
    let field = |name: &str| {
        metadata
            .get(name)
            .or_else(|| metadata.get("model_info").and_then(|info| info.get(name)))
    };
    if field("mode").and_then(serde_json::Value::as_str) == Some("decisions") {
        return Api::Other("openai-decisions".to_owned());
    }
    let providers = metadata
        .get("providers")
        .and_then(serde_json::Value::as_array);
    let provider = metadata
        .pointer("/litellm_params/custom_llm_provider")
        .and_then(serde_json::Value::as_str);
    let backend = metadata
        .pointer("/litellm_params/model")
        .and_then(serde_json::Value::as_str)
        .or_else(|| field("base_model").and_then(serde_json::Value::as_str));
    let openai = if let Some(providers) = providers.filter(|providers| !providers.is_empty()) {
        providers.iter().all(|provider| {
            provider
                .as_str()
                .is_some_and(|provider| provider.eq_ignore_ascii_case("openai"))
        })
    } else if let Some(provider) = provider {
        provider.eq_ignore_ascii_case("openai")
    } else if let Some((provider, _)) = backend.and_then(|backend| backend.split_once('/')) {
        provider.eq_ignore_ascii_case("openai")
    } else {
        let id = id.strip_prefix("openai/").unwrap_or(id);
        builtin_provider("openai").is_some_and(|provider| {
            provider.models.iter().any(|model| {
                model.id == id && model.api == Api::OpenaiResponses && is_chat_model(model)
            })
        })
    };
    if openai {
        Api::OpenaiResponses
    } else {
        Api::OpenaiCompletions
    }
}
