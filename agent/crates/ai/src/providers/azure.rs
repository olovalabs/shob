//! Azure OpenAI Responses uses Responses SSE with Azure's unscoped URL and auth.
use super::openai_responses::OpenAiResponsesProvider;
use crate::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Context, ErrorReason,
    Model, Provider, SimpleStreamOptions, StopReason,
};
use async_trait::async_trait;
use std::collections::BTreeMap;

pub struct AzureProvider {
    id: String,
    api_key: Option<String>,
    inner: OpenAiResponsesProvider,
}
impl AzureProvider {
    pub fn with_models_without_env_api_key(
        id: impl Into<String>,
        api_key: Option<String>,
        http: reqwest::Client,
        models: Vec<Model>,
    ) -> Self {
        let id = id.into();
        Self {
            inner: OpenAiResponsesProvider::with_models_without_env_api_key(
                id.clone(),
                None,
                http,
                models,
            ),
            id,
            api_key,
        }
    }
}
#[async_trait]
impl Provider for AzureProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn models(&self) -> &[Model] {
        self.inner.models()
    }
    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        match prepare(model, opts, self.api_key.as_deref()) {
            Ok((url, headers)) => self
                .inner
                .stream_simple_with_endpoint(model, ctx, opts, url, headers),
            Err(message) => {
                let (mut producer, stream) =
                    crate::event_stream::create_assistant_message_event_stream_with_cancellation(
                        &opts.signal,
                    );
                let mut out = AssistantMessage::empty(
                    model.api.clone(),
                    &model.provider,
                    &model.id,
                    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64,
                );
                out.stop_reason = StopReason::Error;
                out.error_message = Some(message);
                tokio::spawn(async move {
                    producer
                        .push(AssistantMessageEvent::Error {
                            reason: ErrorReason::Error,
                            error: out,
                        })
                        .await;
                });
                stream
            }
        }
    }
}
fn prepare(
    model: &Model,
    opts: &SimpleStreamOptions,
    provider_key: Option<&str>,
) -> Result<(String, BTreeMap<String, String>), String> {
    let mut headers = BTreeMap::new();
    if let Some(key) = opts
        .api_key
        .as_deref()
        .or(provider_key)
        .filter(|v| !v.is_empty())
    {
        headers.insert("api-key".into(), key.into());
    }
    for source in [model.headers.as_ref(), opts.headers.as_ref()]
        .into_iter()
        .flatten()
    {
        for (key, value) in source {
            headers.insert(key.to_ascii_lowercase(), value.clone());
        }
    }
    if !headers
        .iter()
        .any(|(k, v)| matches!(k.as_str(), "api-key" | "authorization") && !v.trim().is_empty())
    {
        return Err("Azure OpenAI API key or Entra Authorization header is required".into());
    }
    let version = headers
        .remove("x-omp-azure-api-version")
        .unwrap_or_else(|| "v1".into());
    let mut url = reqwest::Url::parse(&model.base_url)
        .map_err(|e| format!("Invalid Azure OpenAI base URL: {e}"))?;
    // Unlike OpenAI, Azure appends /responses directly to the configured base;
    // it never inserts /v1 or /deployments/{model}.
    let path = format!("{}/responses", url.path().trim_end_matches('/'));
    url.set_path(&path);
    let query: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "api-version")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.set_query(None);
    url.query_pairs_mut()
        .extend_pairs(query)
        .append_pair("api-version", &version);
    headers.retain(|k, _| !k.starts_with("x-omp-") || k == "x-omp-azure-deployment");
    Ok((url.into(), headers))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn model() -> Model {
        Model::new(
            "gpt-5",
            "GPT",
            crate::Api::AzureOpenaiResponses,
            "azure-openai",
            "https://example.openai.azure.com/openai/v1",
        )
    }
    #[test]
    fn uses_api_key_and_unscoped_responses_url() {
        let (url, headers) =
            prepare(&model(), &SimpleStreamOptions::default(), Some("key")).unwrap();
        assert_eq!(
            url,
            "https://example.openai.azure.com/openai/v1/responses?api-version=v1"
        );
        assert_eq!(headers["api-key"], "key");
        assert!(!headers.contains_key("authorization"));
    }
    #[test]
    fn preserves_gateway_query_and_runtime_auth_overrides() {
        let mut model = model();
        model.base_url = "https://gateway.test/openai?tenant=a&api-version=old".into();
        model.headers = Some(BTreeMap::from([("API-Key".into(), "model-key".into())]));
        let opts = SimpleStreamOptions {
            headers: Some(BTreeMap::from([
                ("api-key".into(), "runtime-key".into()),
                (
                    "X-OMP-Azure-Api-Version".into(),
                    "2025-04-01-preview".into(),
                ),
                ("X-OMP-Azure-Deployment".into(), "deployment".into()),
            ])),
            ..Default::default()
        };
        let (url, headers) = prepare(&model, &opts, Some("default-key")).unwrap();
        assert_eq!(
            url,
            "https://gateway.test/openai/responses?tenant=a&api-version=2025-04-01-preview"
        );
        assert_eq!(headers["api-key"], "runtime-key");
        assert_eq!(headers["x-omp-azure-deployment"], "deployment");
        assert!(!headers.contains_key("x-omp-azure-api-version"));
    }
    #[test]
    fn supports_entra_authorization_without_api_key() {
        let opts = SimpleStreamOptions {
            headers: Some(BTreeMap::from([(
                "Authorization".into(),
                "Bearer entra".into(),
            )])),
            ..Default::default()
        };
        let (_, headers) = prepare(&model(), &opts, None).unwrap();
        assert_eq!(headers["authorization"], "Bearer entra");
        assert!(!headers.contains_key("api-key"));
    }
}
