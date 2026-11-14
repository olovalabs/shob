//! Host-supplied configuration and provider-scoped credentials. No file access.
use rpi_ai::{Api, InputModality, Model, ModelCost, StreamingProtocolCompat, ThinkingLevelMap};
use std::collections::BTreeMap;
use std::path::PathBuf;
pub const DEFAULT_PROVIDER_ID: &str = "anthropic";
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not resolve home directory (set {env} to override)")]
    NoHomeDir { env: &'static str },
    #[error("config dir override {env}={val:?} is not an absolute path")]
    RelativeOverride { env: &'static str, val: String },
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid JSON in {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// A stored credential. Mirrors the TS `Credential` union
/// (`packages/ai/src/auth/types.ts`). OAuth credentials are provider-scoped
/// and must never be logged or exposed to the agent transcript.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Credential {
    /// An API key, optionally sourced from an env var map. v1 stores only the
    /// literal `key` (the `env` field is kept for upstream-shape compatibility).
    ApiKey {
        key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<BTreeMap<String, String>>,
    },
    /// OAuth tokens (access + refresh + expiry).
    Oauth {
        access: String,
        #[serde(default)]
        refresh: String,
        /// Unix epoch seconds. `0` means no known expiry.
        #[serde(default)]
        expires: i64,
    },
}

/// The auth store: `providerId -> Credential`. Mirrors upstream
/// `Record<providerId, Credential>`.
pub type AuthStore = BTreeMap<String, Credential>;

/// The `models.json` document. Mirrors TS `{ providers: Record<id, ProviderConfig> }`
/// (`core/model-config.ts` `ModelsConfigSchema`).
#[derive(serde::Serialize, serde::Deserialize, Default, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ModelsConfig {
    #[serde(default, skip_serializing_if = "indexmap::IndexMap::is_empty")]
    // Provider entries are walked in declaration order, so declaration order
    // participates in the final available-model fallback.
    pub providers: indexmap::IndexMap<String, ProviderConfig>,
}

/// A provider entry in `models.json`. The fields mirror the TS `ProviderConfig`
/// for protocol, endpoint, headers, credentials and model metadata.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    /// `true` ⇒ wrap `api_key` as `Authorization: Bearer <key>` (mirrors
    /// upstream `provider-composer.ts` `authHeader`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_header: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ModelDefinition>,
}

/// One model under a provider. `id` is required (mirrors TS `ModelDefinition`).
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ModelDefinition {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// USD per million tokens, including optional long-context pricing tiers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<ModelCost>,
    /// Free-form modality strings ("text"/"image"); unknown values fall back
    /// to text-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<serde_json::Value>,
}

pub fn model_compat_warnings(config: &ModelsConfig) -> Vec<String> {
    let mut warnings = Vec::new();
    for (provider, cfg) in &config.providers {
        let defaults = match cfg.api.as_deref() {
            Some("openai-completions") => {
                serde_json::to_value(rpi_ai::model::OpenaiCompletionsCompat::default())
                    .expect("compat defaults serialize")
            }
            Some("openai-responses") => {
                serde_json::to_value(rpi_ai::model::OpenaiResponsesCompat::default())
                    .expect("compat defaults serialize")
            }
            _ if provider_is_anthropic_compatible(cfg) => {
                serde_json::to_value(rpi_ai::model::AnthropicMessagesCompat::default())
                    .expect("compat defaults serialize")
            }
            _ => continue,
        };
        for model in &cfg.models {
            let Some(value) = &model.compat else { continue };
            let context = format!("provider {provider:?}, model {:?}, compat", model.id);
            if let Some(fields) = value.as_object() {
                if provider_is_openai_completions(cfg) {
                    if let (Some(native), Some(alias)) = (
                        fields.get("requiresReasoningContent"),
                        fields.get("requiresReasoningContentOnAssistantMessages"),
                    ) {
                        if native != alias {
                            warnings.push(format!("{context}: conflicting reasoning-content flags; requiresReasoningContent takes precedence"));
                        }
                    }
                }
                for key in fields.keys() {
                    let alias = provider_is_openai_completions(cfg)
                        && key == "requiresReasoningContentOnAssistantMessages";
                    if defaults.get(key).is_none() && !alias {
                        warnings.push(format!("{context}: unknown field {key:?}; ignored"));
                    }
                }
            }
            let parsed = if provider_is_openai_completions(cfg) {
                serde_json::from_value::<rpi_ai::model::OpenaiCompletionsCompat>(
                    normalize_completions_compat(value.clone()),
                )
                .map(|_| ())
            } else if provider_is_anthropic_compatible(cfg) {
                serde_json::from_value::<rpi_ai::model::AnthropicMessagesCompat>(value.clone())
                    .map(|_| ())
            } else {
                serde_json::from_value::<rpi_ai::model::OpenaiResponsesCompat>(value.clone())
                    .map(|_| ())
            };
            if let Err(error) = parsed {
                // Serde's full error may echo a malformed value, including a
                // credential accidentally pasted into a boolean field.
                let error = error.to_string();
                let expected = error
                    .rsplit_once(", expected ")
                    .map_or("a valid compat object", |(_, expected)| expected);
                warnings.push(format!(
                    "{context}: invalid configuration; expected {expected}; compat ignored"
                ));
            }
        }
    }
    warnings
}

fn normalize_completions_compat(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(fields) = value.as_object_mut() {
        if let Some(alias) = fields.remove("requiresReasoningContentOnAssistantMessages") {
            fields.entry("requiresReasoningContent").or_insert(alias);
        }
    }
    value
}

pub fn provider_is_anthropic_compatible(cfg: &ProviderConfig) -> bool {
    matches!(
        cfg.api.as_deref(),
        None | Some("") | Some("anthropic-messages")
    )
}

/// Whether a configured provider uses the OpenAI Chat Completions protocol.
pub fn provider_is_openai_completions(cfg: &ProviderConfig) -> bool {
    matches!(cfg.api.as_deref(), Some("openai-completions"))
}

pub fn provider_is_openai_responses(cfg: &ProviderConfig) -> bool {
    matches!(cfg.api.as_deref(), Some("openai-responses"))
}

pub fn provider_to_models(
    provider_id: &str,
    cfg: &ProviderConfig,
    values: &dyn crate::ValueSource,
) -> Option<Vec<Model>> {
    let api = if provider_is_anthropic_compatible(cfg) {
        Api::AnthropicMessages
    } else if provider_is_openai_completions(cfg) {
        Api::OpenaiCompletions
    } else if provider_is_openai_responses(cfg) {
        Api::OpenaiResponses
    } else {
        serde_json::from_value(serde_json::Value::String(cfg.api.clone()?)).ok()?
    };
    let provider_base = cfg
        .base_url
        .clone()
        .or_else(|| {
            crate::catalog::builtin_provider(provider_id).map(|provider| {
                provider
                    .base_url_env
                    .as_ref()
                    .and_then(|name| values.env(name))
                    .unwrap_or_else(|| provider.base_url.clone())
            })
        })
        .unwrap_or_else(|| match api {
            Api::OpenaiCompletions | Api::OpenaiResponses => "https://api.openai.com".to_string(),
            _ => default_anthropic_base_url(),
        });
    let mut merged: Vec<Model> = Vec::with_capacity(cfg.models.len());
    for def in &cfg.models {
        let base_url = def
            .base_url
            .clone()
            .unwrap_or_else(|| provider_base.clone());
        let name = def.name.clone().unwrap_or_else(|| def.id.clone());
        let mut m = Model::new(
            def.id.clone(),
            name,
            api.clone(),
            provider_id.to_string(),
            base_url,
        );
        m.reasoning = def.reasoning.unwrap_or(false);
        m.thinking_level_map = def.thinking_level_map.clone();
        m.context_window = def.context_window.unwrap_or(0);
        m.max_tokens = def.max_tokens.unwrap_or(0);
        if let Some(cost) = &def.cost {
            m.cost = cost.clone();
            m.cost_configured = true;
        }
        m.input = parse_input_modalities(def.input.as_deref());
        // Merge: model-level headers, then provider-level headers (provider wins
        // on conflict — it's the more specific-to-this-endpoint declaration).
        // Values are resolved via `resolve_headers` (`$ENV`/`!command` expansion,
        // mirroring pi's `resolveHeadersOrThrow`) so a copied pi models.json
        // referencing env vars / commands resolves the same way. Models.json
        // providers have no credential env overlay (only auth.json keys do), so
        // the expansion is env-only here.
        // Credential-derived headers are NOT synthesized in the catalog —
        // [`crate::RuntimeConfig::resolve`] applies it centrally so it can skip it
        // when a higher-priority x-api-key source (`--api-key` / auth.json /
        // `ANTHROPIC_API_KEY`) wins. Folding it here unconditionally would put a
        // Bearer on the model even on the x-api-key path. See
        // `models_json_bearer_token` + the fold loop in `resolve`.
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        if let Some(h) = def.headers.clone() {
            for (k, v) in crate::values::resolve_headers(&h, None, values) {
                headers.insert(k, v);
            }
        }
        if let Some(h) = cfg.headers.clone() {
            for (k, v) in crate::values::resolve_headers(&h, None, values) {
                headers.insert(k, v);
            }
        }
        if let Some(value) = def.compat.clone() {
            match api {
                Api::OpenaiResponses => {
                    if let Ok(compat) = serde_json::from_value(value) {
                        m.compat = Some(StreamingProtocolCompat::OpenaiResponses(compat));
                    }
                }
                Api::OpenaiCompletions => {
                    if let Ok(compat) = serde_json::from_value(normalize_completions_compat(value))
                    {
                        m.compat = Some(StreamingProtocolCompat::OpenaiCompletions(compat));
                    }
                }
                Api::AnthropicMessages => {
                    if let Ok(compat) = serde_json::from_value(value) {
                        m.compat = Some(StreamingProtocolCompat::AnthropicMessages(compat));
                    }
                }
                _ => {}
            }
        }
        if !headers.is_empty() {
            m.headers = Some(headers);
        }
        merged.push(m);
    }
    Some(merged)
}

/// Parse `["text","image"]`-style modality strings into [`InputModality`]s;
/// unknown values drop to text-only. `None` ⇒ text (the [`Model::new`] default).
fn parse_input_modalities(input: Option<&[String]>) -> Vec<InputModality> {
    match input {
        None => vec![InputModality::Text],
        Some([]) => vec![InputModality::Text],
        Some(list) => list
            .iter()
            .filter_map(|s| match s.to_ascii_lowercase().as_str() {
                "text" => Some(InputModality::Text),
                "image" => Some(InputModality::Image),
                _ => None,
            })
            .collect::<Vec<_>>()
            .pipe(|v| {
                if v.is_empty() {
                    vec![InputModality::Text]
                } else {
                    v
                }
            }),
    }
}

/// The first-party Anthropic endpoint — used as the fallback `base_url` when a
/// models.json provider omits it. Kept here (not imported from `rpi_ai`) so the
/// config layer never depends on the provider's private `models` module.
/// Public so [`crate::RuntimeConfig::resolve`] can tell a gateway model (whose
/// `base_url` differs from this) from a built-in Anthropic model.
pub const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Same value as [`ANTHROPIC_DEFAULT_BASE_URL`], as an owned `String` for the
/// `unwrap_or_else` ergonomic used by [`provider_to_models`].
pub fn default_anthropic_base_url() -> String {
    ANTHROPIC_DEFAULT_BASE_URL.to_string()
}

trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}
impl<T> Pipe for T {}
