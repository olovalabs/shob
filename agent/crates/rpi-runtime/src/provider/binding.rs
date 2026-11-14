//! Private execution state. Public model snapshots never carry request headers
//! or credentials; requests recover their configuration by model identity here.
use super::*;
use crate::credentials::{oauth_needs_refresh, OAuthRefresher};
use std::sync::{PoisonError, RwLock};

#[derive(Clone, Default)]
pub(super) enum RequestAuth {
    #[default]
    None,
    ApiKey(String),
    Headers(BTreeMap<String, String>),
}

impl RequestAuth {
    /// Whether `token` is the credential this auth sends.
    pub fn carries(&self, token: &str) -> bool {
        match self {
            Self::None => false,
            Self::ApiKey(key) => key == token,
            Self::Headers(headers) => headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("authorization")
                    && value.strip_prefix("Bearer ") == Some(token)
            }),
        }
    }

    /// Swap in a renewed token, keeping how it is sent.
    fn renew(&mut self, token: &str) {
        match self {
            Self::None => {}
            Self::ApiKey(key) => *key = token.into(),
            Self::Headers(headers) => {
                for (name, value) in headers.iter_mut() {
                    if name.eq_ignore_ascii_case("authorization") {
                        *value = format!("Bearer {token}");
                    }
                }
            }
        }
    }
}

/// Renewal state for auth bound from the host's stored OAuth credential.
pub(super) struct OAuthLease {
    refresher: Arc<dyn OAuthRefresher>,
    /// Serializes renewals; concurrent requests wait and reuse the result.
    expires: tokio::sync::Mutex<i64>,
}

impl OAuthLease {
    pub fn new(refresher: Arc<dyn OAuthRefresher>, expires: i64) -> Self {
        Self {
            refresher,
            expires: tokio::sync::Mutex::new(expires),
        }
    }
}

pub(super) struct ExecutionBinding {
    provider_id: String,
    models: Vec<Model>,
    descriptors: Vec<Model>,
    auth: RwLock<RequestAuth>,
    oauth: Option<OAuthLease>,
}

impl std::fmt::Debug for ExecutionBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionBinding")
            .field("provider", &self.provider_id)
            .field("models", &self.models.len())
            .field("auth", &"[redacted]")
            .finish()
    }
}

pub(super) fn descriptor(model: &Model) -> Model {
    let mut model = model.clone();
    model.headers = None;
    // Endpoint userinfo, queries and fragments can carry credentials too. Only
    // the private binding needs the complete URL; public routing uses identity.
    if let Ok(mut url) = reqwest::Url::parse(&model.base_url) {
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            model.base_url = url.into();
        }
    }
    model
}

impl ExecutionBinding {
    pub fn new(
        provider_id: &str,
        models: Vec<Model>,
        auth: RequestAuth,
        oauth: Option<OAuthLease>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            descriptors: models.iter().map(descriptor).collect(),
            models,
            auth: RwLock::new(auth),
            oauth,
        }
    }

    pub fn descriptors(&self) -> &[Model] {
        &self.descriptors
    }

    fn bound_model(&self, requested: &Model) -> Option<&Model> {
        if requested.provider != self.provider_id {
            return None;
        }
        let candidates = || {
            self.models.iter().filter(|model| {
                model.provider == requested.provider
                    && model.api == requested.api
                    && model.id.eq_ignore_ascii_case(&requested.id)
            })
        };
        candidates()
            .find(|model| model.id == requested.id)
            .or_else(|| {
                let mut candidates = candidates();
                let first = candidates.next()?;
                candidates.next().is_none().then_some(first)
            })
    }

    pub fn binds(&self, requested: &Model) -> bool {
        self.bound_model(requested).is_some()
    }

    /// Renew a stored OAuth credential that has expired or is about to, before
    /// it is sent. An explicit request key bypasses the binding entirely.
    pub async fn renew_oauth(&self, options: &SimpleStreamOptions) -> Result<(), String> {
        let Some(lease) = self.oauth.as_ref().filter(|_| options.api_key.is_none()) else {
            return Ok(());
        };
        if matches!(
            &*self.auth.read().unwrap_or_else(PoisonError::into_inner),
            RequestAuth::Headers(_)
        ) && options.headers.as_ref().is_some_and(|headers| {
            headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("authorization"))
        }) {
            return Ok(());
        }
        let mut expires = lease.expires.lock().await;
        if !oauth_needs_refresh(*expires, now_unix_seconds()) {
            return Ok(());
        }
        let refresher = Arc::clone(&lease.refresher);
        let provider = self.provider_id.clone();
        let renewed = tokio::task::spawn_blocking(move || refresher.refresh(&provider))
            .await
            .map_err(|_| "the refresh stopped unexpectedly".to_owned())??;
        if renewed.access.trim().is_empty()
            || (renewed.expires != 0 && renewed.expires <= now_unix_seconds())
        {
            return Err("the provider returned an invalid or expired credential".into());
        }
        self.auth
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .renew(&renewed.access);
        *expires = renewed.expires;
        Ok(())
    }

    pub fn prepare(
        &self,
        requested: &Model,
        options: &SimpleStreamOptions,
    ) -> Option<(Model, SimpleStreamOptions)> {
        // The caller may select a published model, but cannot substitute an
        // endpoint or headers to redirect an already-bound credential.
        let model = self.bound_model(requested)?.clone();
        let mut options = options.clone();
        if options.api_key.is_none() {
            match &*self.auth.read().unwrap_or_else(PoisonError::into_inner) {
                RequestAuth::None => {}
                RequestAuth::ApiKey(key) => options.api_key = Some(key.clone()),
                RequestAuth::Headers(headers) => {
                    let mut merged = headers.clone();
                    for (name, value) in options.headers.take().unwrap_or_default() {
                        merged.retain(|known, _| !known.eq_ignore_ascii_case(&name));
                        merged.insert(name, value);
                    }
                    options.headers = Some(merged);
                }
            }
        }
        Some((model, options))
    }

    #[cfg(test)]
    pub fn request_model(&self, requested: &Model) -> Model {
        let (mut model, options) = self
            .prepare(requested, &SimpleStreamOptions::default())
            .unwrap();
        if let Some(headers) = &options.headers {
            merge_auth_headers(&mut model, headers);
        }
        if model.api != rpi_ai::Api::AnthropicMessages {
            if let Some(key) = options.api_key {
                merge_auth_headers(
                    &mut model,
                    &BTreeMap::from([("authorization".into(), format!("Bearer {key}"))]),
                );
            }
        }
        model
    }
}

fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(i64::MAX, |duration| duration.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::RefreshedOAuth;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Refresher(AtomicUsize);
    impl OAuthRefresher for Refresher {
        fn supports(&self, _: &str) -> bool {
            true
        }
        fn refresh(&self, _: &str) -> Result<RefreshedOAuth, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(RefreshedOAuth {
                access: "renewed-secret".into(),
                expires: 0,
            })
        }
    }

    #[tokio::test]
    async fn renewal_after_first_request_is_shared_by_concurrent_requests() {
        let refresher = Arc::new(Refresher(AtomicUsize::new(0)));
        let model = Model::new(
            "model",
            "Model",
            rpi_ai::Api::OpenaiCompletions,
            "account",
            "https://example.com",
        );
        let binding = ExecutionBinding::new(
            "account",
            vec![model.clone()],
            RequestAuth::ApiKey("old-secret".into()),
            Some(OAuthLease::new(refresher.clone(), 0)),
        );
        let options = SimpleStreamOptions::default();
        binding.renew_oauth(&options).await.unwrap();
        assert_eq!(
            binding
                .prepare(&model, &options)
                .unwrap()
                .1
                .api_key
                .as_deref(),
            Some("old-secret")
        );
        *binding.oauth.as_ref().unwrap().expires.lock().await = 1;
        let (a, b) = tokio::join!(binding.renew_oauth(&options), binding.renew_oauth(&options));
        a.unwrap();
        b.unwrap();
        assert_eq!(refresher.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            binding
                .prepare(&model, &options)
                .unwrap()
                .1
                .api_key
                .as_deref(),
            Some("renewed-secret")
        );
        assert!(!serde_json::to_string(binding.descriptors())
            .unwrap()
            .contains("secret"));
    }
}
