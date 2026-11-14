//! Mirrors `packages/ai/src/api/anthropic-messages.ts` + `providers/anthropic.ts`
//! — the Anthropic Messages-API provider: request building, SSE decoding,
//! event mapping, retry, and cost. The crate-level [`Provider`] impl lives here.
//!
//! Only compiled when the `providers` feature is on (reqwest/bytes/eventsource-
//! stream are optional deps). The smoke test behind the `smoke` feature drives
//! a live request against `ANTHROPIC_API_KEY`.
//!
//! API-key and subscription OAuth authentication share the Messages transport.
//! OAuth adds the required bearer, beta, CLI identity, and system identity.

pub mod build_params;
pub mod cache_control;
pub mod cost;
pub mod json_parse;
pub mod mapper;
pub mod models;
pub mod retry;
pub mod sse;

use crate::error::AiError;
use crate::event_stream::{
    create_assistant_message_event_stream_with_cancellation, AssistantMessageEventStream,
    AssistantMessageEventStreamProducer,
};
use crate::model::{Model, StreamingProtocolCompat};
use crate::provider::{CacheRetention, Provider, SimpleStreamOptions};
use crate::types::{AssistantMessageEvent, Content, Context, DoneReason, StopReason, Usage};
use async_trait::async_trait;
use std::sync::OnceLock;
use std::time::Instant;

use crate::providers::anthropic::build_params::{build_params_prepared, BuiltParams};
use crate::providers::anthropic::cost::calculate_cost;
use crate::providers::anthropic::mapper::{
    emit_terminal_error, run_mapper, run_mapper_with_diagnostics, MapperDiagnostics, MapperState,
};
use crate::providers::anthropic::models::anthropic_models;
use crate::providers::anthropic::retry::retry_provider_request;
use crate::providers::anthropic::sse::SseEventStream;

/// The fixed Anthropic Messages-API version header. Mirrors the SDK default
/// (`2023-06-01`); the TS SDK attaches this implicitly, the Rust port sets it
/// explicitly since it speaks raw `reqwest`.
const ANTHROPIC_VERSION: &str = "2023-06-01";

static PROVIDER_DIAGNOSTICS: OnceLock<bool> = OnceLock::new();
static PROVIDER_DIAGNOSTICS_DETAIL: OnceLock<bool> = OnceLock::new();

fn provider_diagnostics_enabled() -> bool {
    *PROVIDER_DIAGNOSTICS.get_or_init(|| {
        std::env::var_os("RPI_PROVIDER_DIAGNOSTICS")
            .or_else(|| std::env::var_os("RPI_AGENT_DIAGNOSTICS"))
            .is_some_and(|value| value != "0")
    })
}

fn provider_diagnostics_detail_enabled() -> bool {
    *PROVIDER_DIAGNOSTICS_DETAIL.get_or_init(|| {
        provider_diagnostics_enabled()
            && std::env::var_os("RPI_PROVIDER_DIAGNOSTICS_DETAIL").is_some_and(|value| value != "0")
    })
}

struct ProviderRequestDiagnostics {
    enabled: bool,
    detail: bool,
    started: Option<Instant>,
    params_build_ns: u64,
    normalize_ns: u64,
    deferred_tools_ns: u64,
    convert_messages_ns: u64,
    convert_tools_ns: u64,
    context_tokens_ns: u64,
    preencoded_json_ns: u64,
    build_total_ns: u64,
    request_encode_ns: u64,
    request_json_ns: u64,
    request_build_ns: u64,
    request_body_bytes: usize,
    messages_bytes: usize,
    tools_bytes: usize,
    system_bytes: usize,
    message_count: usize,
    http_wait_ns: u64,
    sse_events: u64,
    mapper_ns: u64,
    mapper_pull_ns: u64,
    mapper_parse_ns: u64,
    mapper_apply_ns: u64,
    sse_first_event_ns: u64,
}

impl ProviderRequestDiagnostics {
    fn new() -> Self {
        let enabled = provider_diagnostics_enabled();
        Self {
            enabled,
            detail: provider_diagnostics_detail_enabled(),
            started: enabled.then(Instant::now),
            params_build_ns: 0,
            normalize_ns: 0,
            deferred_tools_ns: 0,
            convert_messages_ns: 0,
            convert_tools_ns: 0,
            context_tokens_ns: 0,
            preencoded_json_ns: 0,
            build_total_ns: 0,
            request_encode_ns: 0,
            request_json_ns: 0,
            request_build_ns: 0,
            request_body_bytes: 0,
            messages_bytes: 0,
            tools_bytes: 0,
            system_bytes: 0,
            message_count: 0,
            http_wait_ns: 0,
            sse_events: 0,
            mapper_ns: 0,
            mapper_pull_ns: 0,
            mapper_parse_ns: 0,
            mapper_apply_ns: 0,
            sse_first_event_ns: 0,
        }
    }

    fn elapsed(started: Option<Instant>) -> u64 {
        started
            .map(|started| started.elapsed().as_nanos().min(u64::MAX as u128) as u64)
            .unwrap_or_default()
    }
}

impl Drop for ProviderRequestDiagnostics {
    fn drop(&mut self) {
        if !self.enabled {
            return;
        }
        eprintln!(
            "RPI_PROVIDER_DIAGNOSTICS provider=anthropic detail={} params_build_ns={} normalize_ns={} deferred_tools_ns={} convert_messages_ns={} convert_tools_ns={} context_tokens_ns={} preencoded_json_ns={} build_total_ns={} request_encode_ns={} request_json_ns={} request_build_ns={} request_body_bytes={} messages_bytes={} tools_bytes={} system_bytes={} message_count={} http_wait_ns={} sse_events={} mapper_ns={} mapper_pull_ns={} mapper_parse_ns={} mapper_apply_ns={} sse_first_event_ns={} total_ns={}",
            self.detail,
            self.params_build_ns,
            self.normalize_ns,
            self.deferred_tools_ns,
            self.convert_messages_ns,
            self.convert_tools_ns,
            self.context_tokens_ns,
            self.preencoded_json_ns,
            self.build_total_ns,
            self.request_encode_ns,
            self.request_json_ns,
            self.request_build_ns,
            self.request_body_bytes,
            self.messages_bytes,
            self.tools_bytes,
            self.system_bytes,
            self.message_count,
            self.http_wait_ns,
            self.sse_events,
            self.mapper_ns,
            self.mapper_pull_ns,
            self.mapper_parse_ns,
            self.mapper_apply_ns,
            self.sse_first_event_ns,
            Self::elapsed(self.started),
        );
    }
}

/// The Anthropic Messages-API provider. Mirrors the `Anthropic`-SDK-backed
/// surface in `api/anthropic-messages.ts` (`stream` + `streamSimple`) +
/// `providers/anthropic.ts`.
///
/// Supports API keys, subscription OAuth bearer tokens, and gateway-owned
/// authentication headers.
/// Construct via [`AnthropicProvider::new`] or [`AnthropicProvider::from_env`].
pub struct AnthropicProvider {
    /// Default API key, used when `SimpleStreamOptions::api_key` is `None`.
    /// `None` here means "defer to opts + env at call time".
    api_key: Option<String>,
    /// Whether a missing explicit/default key may fall back to the built-in
    /// Anthropic environment variable. Custom provider identities must disable
    /// this or an official Anthropic key could be sent to their endpoint.
    allow_env_api_key: bool,
    require_auth: bool,
    http: reqwest::Client,
    models: Vec<Model>,
}

impl AnthropicProvider {
    /// Build a provider with an explicit default API key + custom
    /// `reqwest::Client` (useful for proxies / custom TLS). Serves the
    /// hand-curated catalog from [`models::anthropic_models`].
    pub fn new(api_key: Option<String>, http: reqwest::Client) -> Self {
        Self {
            api_key,
            allow_env_api_key: true,
            require_auth: true,
            http,
            models: anthropic_models(),
        }
    }

    /// Build with a custom model catalog (e.g. a generated models.dev snapshot).
    pub fn with_models(api_key: Option<String>, http: reqwest::Client, models: Vec<Model>) -> Self {
        Self {
            api_key,
            allow_env_api_key: true,
            require_auth: true,
            http,
            models,
        }
    }

    /// Build an Anthropic-wire provider for a non-Anthropic identity.
    ///
    /// Unlike [`Self::with_models`], this never consults `ANTHROPIC_API_KEY`
    /// when the provider has no explicit key. Authentication must come from
    /// the supplied provider key or request/model headers, preserving the
    /// provider id as a credential boundary.
    pub fn with_models_without_env_api_key(
        api_key: Option<String>,
        http: reqwest::Client,
        models: Vec<Model>,
    ) -> Self {
        Self {
            api_key,
            allow_env_api_key: false,
            require_auth: true,
            http,
            models,
        }
    }

    /// Explicit anonymous authentication for catalog-approved gateway routes.
    pub fn with_models_without_auth(http: reqwest::Client, models: Vec<Model>) -> Self {
        let mut provider = Self::with_models_without_env_api_key(None, http, models);
        provider.require_auth = false;
        provider
    }

    /// Convenience: build with a default `reqwest::Client`, deferring API-key
    /// resolution to call time (opts.api_key → provider default →
    /// `ANTHROPIC_API_KEY` env). Mirrors how the smoke test constructs a
    /// provider without a pre-resolved key.
    pub fn from_env() -> Self {
        // httpx-style: honor HTTP_PROXY/HTTPS_PROXY/NO_PROXY + idle timeout.
        let http = crate::http::build_client("https://api.anthropic.com", None, None)
            .unwrap_or_else(|_| reqwest::Client::new());
        Self::new(None, http)
    }

    /// Default API key carried on the provider (`opts.api_key` still wins).
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn models(&self) -> &[Model] {
        &self.models
    }

    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        // Prepare owned wire parameters from the borrowed context before the
        // background task starts. It needs the request, not a second deep copy
        // of the entire history. Failures still surface as stream Error events.
        let (mut prod, stream) =
            create_assistant_message_event_stream_with_cancellation(&opts.signal);

        let http = self.http.clone();
        let params_started = provider_diagnostics_enabled().then(Instant::now);
        let params = prepare_params(
            model,
            ctx,
            opts,
            &self.api_key,
            self.allow_env_api_key,
            self.require_auth,
        );
        let params_build_ns = params_started
            .map(|started| started.elapsed().as_nanos().min(u64::MAX as u128) as u64)
            .unwrap_or_default();
        let model = model.clone();
        let mut opts = opts.clone();
        opts.signal = prod.cancellation_token();

        tokio::spawn(async move {
            run_anthropic_stream(&mut prod, http, &model, params, &opts, params_build_ns).await;
            // `prod` drops here, closing the mpsc sender so the consumer's
            // `next()` returns `None` after the terminal event.
        });

        stream
    }
}

// ----------------------------------------------------------------------------
// Producer task — mirrors the TS `stream` async IIFE (anthropic-messages.ts
// :487-774) + `streamSimple` (:801-841).
// ----------------------------------------------------------------------------

/// The producer task body. Mirrors the TS `stream` async IIFE: build the empty
/// `output`, resolve auth (`assertRequestAuth`), build params, POST with retry
/// (`retryProviderRequest`), then drive the SSE mapper to the terminal event.
/// Every failure path pushes an `Error` terminal so the consumer's `result()`
/// always resolves.
///
/// The TS collapses `streamSimple` → `stream` → `buildParams` with an
/// intermediate `AnthropicOptions`; the Rust port folds `streamSimple` into
/// this function and lets `build_params` read `SimpleStreamOptions` directly
/// (the adaptive-vs-budget thinking decision lives there), so there is no
/// intermediate options shape.
struct PreparedParams {
    built: BuiltParams,
    api_key_for_header: Option<String>,
}

fn prepare_params(
    model: &Model,
    ctx: &Context,
    opts: &SimpleStreamOptions,
    provider_key: &Option<String>,
    allow_env_api_key: bool,
    require_auth: bool,
) -> Result<PreparedParams, String> {
    // ---- resolve auth (assertRequestAuth) ----
    let resolved_key = resolve_api_key(provider_key, opts, allow_env_api_key);
    let is_oauth = resolved_key
        .as_deref()
        .is_some_and(|key| key.starts_with("sk-ant-oat"))
        || [model.headers.as_ref(), opts.headers.as_ref()]
            .into_iter()
            .flatten()
            .any(|headers| {
                headers.iter().any(|(name, value)| {
                    name.eq_ignore_ascii_case("authorization")
                        && value
                            .strip_prefix("Bearer ")
                            .is_some_and(|key| key.starts_with("sk-ant-oat"))
                })
            });
    // TS `assertRequestAuth(model.provider, options?.apiKey, options?.headers)`
    // checks `options.headers` alone — but upstream the model-resolution layer
    // has *already merged* `model.headers` into `options.headers` (models.ts
    // `getApiKeyAndHeaders` → `auth.headers = merge(..., providerOrModel.headers)`).
    // The Rust port keeps auth headers on `model.headers` and merges them later
    // in `assemble_headers`, so here the check must consider BOTH sources to
    // reproduce the TS net effect. This is what lets a Bearer header folded onto
    // `model.headers` (the `ANTHROPIC_AUTH_TOKEN` / `models.json authHeader:true`
    // paths in rpi-cli's `provider::resolve`) authenticate without an `x-api-key`.
    let header_owned_auth = has_header_auth(&opts.headers) || has_header_auth(&model.headers);
    let api_key_for_header = match (resolved_key, header_owned_auth) {
        (Some(k), _) => Some(k),
        (None, true) => None, // headers carry auth; do not send x-api-key.
        (None, false) if !require_auth => None,
        (None, false) => {
            // Mirrors `assertRequestAuth`'s `throw new Error("No API key for…")`.
            return Err(format!("No API key for provider: {}", model.provider));
        }
    };

    // Subscription OAuth uses the same Messages stream, with its required identity.
    Ok(PreparedParams {
        built: build_params_prepared(
            model,
            ctx,
            is_oauth,
            opts,
            !is_vertex_anthropic(model)
                && std::env::var("RPI_ANTHROPIC_NON_STREAM").ok().as_deref() != Some("1"),
        ),
        api_key_for_header,
    })
}

async fn run_anthropic_stream(
    prod: &mut AssistantMessageEventStreamProducer,
    http: reqwest::Client,
    model: &Model,
    params: Result<PreparedParams, String>,
    opts: &SimpleStreamOptions,
    params_build_ns: u64,
) {
    let mut diagnostics = ProviderRequestDiagnostics::new();
    diagnostics.params_build_ns = params_build_ns;
    let mut state = MapperState::new(
        model.api.clone(),
        model.provider.clone(),
        model.id.clone(),
        now_ms(),
    );
    let PreparedParams {
        mut built,
        api_key_for_header,
    } = match params {
        Ok(params) => params,
        Err(error) => {
            emit_terminal_error(prod, &mut state, error, false).await;
            return;
        }
    };
    if diagnostics.enabled {
        diagnostics.normalize_ns = built.diagnostics.normalize_ns;
        diagnostics.deferred_tools_ns = built.diagnostics.deferred_tools_ns;
        diagnostics.convert_messages_ns = built.diagnostics.convert_messages_ns;
        diagnostics.convert_tools_ns = built.diagnostics.convert_tools_ns;
        diagnostics.context_tokens_ns = built.diagnostics.context_tokens_ns;
        diagnostics.preencoded_json_ns = built.diagnostics.preencoded_json_ns;
        diagnostics.build_total_ns = built.diagnostics.total_ns;
    }
    let non_stream = std::env::var("RPI_ANTHROPIC_NON_STREAM").ok().as_deref() == Some("1");
    if diagnostics.detail {
        diagnostics.messages_bytes = serde_json::to_vec(&built.request.messages)
            .map(|value| value.len())
            .unwrap_or_default();
        diagnostics.tools_bytes = serde_json::to_vec(&built.request.tools)
            .map(|value| value.len())
            .unwrap_or_default();
        diagnostics.system_bytes = serde_json::to_vec(&built.request.system)
            .map(|value| value.len())
            .unwrap_or_default();
        diagnostics.message_count = built.request.messages.len();
    } else if diagnostics.enabled {
        diagnostics.message_count = if built.preencoded_body.is_some() {
            built.preencoded_message_count
        } else {
            built.request.messages.len()
        };
    }
    let vertex_anthropic = is_vertex_anthropic(model);
    let preencoded_body = if non_stream || vertex_anthropic {
        None
    } else {
        built.preencoded_body.take()
    };

    // ---- assemble headers (createClient, API-key arm) ----
    let mut headers = assemble_headers(
        model,
        opts,
        built.beta_header.as_deref(),
        api_key_for_header.as_deref(),
    );

    let url = if vertex_anthropic {
        match vertex_anthropic_url(model, opts) {
            Ok(url) => url,
            Err(error) => {
                emit_terminal_error(prod, &mut state, error, false).await;
                return;
            }
        }
    } else {
        anthropic_messages_url(&model.base_url)
    };
    if vertex_anthropic {
        headers.retain(|(name, _)| {
            !matches!(
                name.to_ascii_lowercase().as_str(),
                "x-omp-google-project" | "x-goog-location" | "anthropic-beta"
            )
        });
    }
    let timeout = opts.request_timeout();
    let signal = opts.signal.clone();
    let has_content_type = headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-type"));
    let mut builder = http.post(&url).timeout(timeout);
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    // Normal streaming requests use the pre-encoded borrowed body when the
    // canonical text fast path produced one. Only the compatibility mode needs
    // a mutable JSON tree before the single encoding.
    let encode_started = diagnostics.enabled.then(Instant::now);
    let mut request_parts = super::request::PreparedJsonRequestDiagnostics::default();
    let prepared = if vertex_anthropic {
        serde_json::to_value(&built.request)
            .map_err(|error| error.to_string())
            .and_then(|mut body| {
                shape_vertex_anthropic_body(&mut body, built.beta_header.as_deref());
                if non_stream {
                    body["stream"] = serde_json::Value::Bool(false);
                }
                super::request::PreparedJsonRequest::new_with_diagnostics(
                    builder,
                    &body,
                    diagnostics.enabled.then_some(&mut request_parts),
                )
                .map_err(|error| error.to_string())
            })
    } else if let Some(body) = preencoded_body {
        super::request::PreparedJsonRequest::new_body(builder, body, !has_content_type)
            .map_err(|error| error.to_string())
    } else if non_stream {
        serde_json::to_value(&built.request)
            .map_err(|error| error.to_string())
            .and_then(|mut body| {
                body["stream"] = serde_json::Value::Bool(false);
                strip_cache_control(&mut body);
                simplify_non_stream_request(&mut body);
                super::request::PreparedJsonRequest::new_with_diagnostics(
                    builder,
                    &body,
                    diagnostics.enabled.then_some(&mut request_parts),
                )
                .map_err(|error| error.to_string())
            })
    } else {
        super::request::PreparedJsonRequest::new_with_diagnostics(
            builder,
            &built.request,
            diagnostics.enabled.then_some(&mut request_parts),
        )
        .map_err(|error| error.to_string())
    };
    if diagnostics.enabled {
        diagnostics.request_encode_ns = ProviderRequestDiagnostics::elapsed(encode_started);
        diagnostics.request_json_ns = request_parts.json_serialize_ns;
        diagnostics.request_build_ns = request_parts.request_build_ns;
    }
    drop(built);
    let request = match prepared {
        Ok(request) => {
            if diagnostics.enabled {
                diagnostics.request_body_bytes = request.body_len().unwrap_or_default();
            }
            request
        }
        Err(error) => {
            emit_terminal_error(
                prod,
                &mut state,
                format!("failed to serialize request body: {error}"),
                false,
            )
            .await;
            return;
        }
    };

    // ---- POST with retry (retryProviderRequest) ----
    // The TS hands the SDK `.create({...params, stream:true}, {signal, timeout,
    // maxRetries:0}).asResponse()` to retryProviderRequest; the Rust port
    // reproduces that as a raw `reqwest` POST that returns a `Response` on
    // 2xx and an `AiError::Http { status }` on non-2xx (so the retry predicate
    // can classify 408/409/429/>=500). Cancellation is honored via a `select!`
    // around the send — the in-flight reqwest future is dropped on abort.
    let http_started = diagnostics.enabled.then(Instant::now);
    let response_result = retry_provider_request(
        move || {
            let request = request.clone();
            let signal = signal.clone();
            async move {
                let resp = tokio::select! {
                    r = request.send() => r?,
                    _ = signal.cancelled() => return Err(AiError::Abort {
                        message: "Request aborted".to_string(),
                    }),
                };
                let status = resp.status();
                if !status.is_success() {
                    let code = status.as_u16();
                    // Read the error body for a diagnostic message; the retry
                    // predicate only needs the status code.
                    let text = tokio::select! {
                        biased;
                        _ = signal.cancelled() => return Err(AiError::Abort {
                            message: "Request aborted".to_string(),
                        }),
                        result = resp.text() => result.map_err(|error| AiError::Http {
                            status: Some(code),
                            message: format!("error reading HTTP error body: {error}"),
                        })?,
                    };
                    return Err(AiError::Http {
                        status: Some(code),
                        message: text,
                    });
                }
                Ok(resp)
            }
        },
        opts.max_retries,
        opts.max_retry_delay,
        &opts.signal,
    )
    .await;
    if diagnostics.enabled {
        diagnostics.http_wait_ns = ProviderRequestDiagnostics::elapsed(http_started);
    }

    let response = match response_result {
        Ok(r) => r,
        Err(err) => {
            let aborted = err.is_abort();
            // Deliberately NO eprintln! here: in fullscreen TUI mode a raw
            // stderr write lands at the cursor (the input editor row), corrupting
            // the alt-screen. The error is propagated to the UI via
            // `emit_terminal_error` → `AssistantMessageEvent::Error`, which the
            // TUI renders as a `✗` line in the transcript.
            emit_terminal_error(prod, &mut state, err.to_string(), aborted).await;
            return;
        }
    };

    if non_stream {
        let response_body = match tokio::select! {
            biased;
            _ = opts.signal.cancelled() => Err(AiError::Abort {
                message: "Request aborted".to_string(),
            }),
            result = response.text() => result.map_err(|error| AiError::Http {
                status: None,
                message: format!("failed to read non-stream response: {error}"),
            }),
        } {
            Ok(body) => body,
            Err(error) => {
                let aborted = error.is_abort();
                emit_terminal_error(prod, &mut state, error.to_string(), aborted).await;
                return;
            }
        };
        let value: serde_json::Value = match serde_json::from_str(&response_body) {
            Ok(value) => value,
            Err(error) => {
                emit_terminal_error(
                    prod,
                    &mut state,
                    format!("failed to parse non-stream response: {error}"),
                    false,
                )
                .await;
                return;
            }
        };
        let text = value
            .get("content")
            .and_then(serde_json::Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|block| {
                        block.get("type").and_then(serde_json::Value::as_str) == Some("text")
                    })
                    .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
                    .collect::<String>()
            })
            .unwrap_or_default();
        if text.trim().is_empty() {
            emit_terminal_error(
                prod,
                &mut state,
                "non-stream response contained no text content".into(),
                false,
            )
            .await;
            return;
        }
        state.output.content.push(Content::text(text.clone()));
        state.output.response_id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        state.output.response_model = value
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        if let Some(usage) = value.get("usage") {
            state.output.usage.input = usage
                .get("input_tokens")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            state.output.usage.output = usage
                .get("output_tokens")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            state.output.usage.total_tokens = state.output.usage.input + state.output.usage.output;
        }
        state.output.stop_reason = StopReason::Stop;
        let partial = std::sync::Arc::new(state.output.clone());
        prod.push(AssistantMessageEvent::Start {
            partial: partial.clone(),
        })
        .await;
        prod.push(AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: partial.clone(),
        })
        .await;
        prod.push(AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: text.clone(),
            partial: partial.clone(),
        })
        .await;
        prod.push(AssistantMessageEvent::TextEnd {
            content_index: 0,
            content: text,
            partial,
        })
        .await;
        prod.push(AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: state.output,
        })
        .await;
        return;
    }

    // ---- push start, drive SSE → mapper → terminal ----
    // The TS pushes `start` here right before the event loop; the Rust mapper
    // defers `start` to the first event (`ensure_started`) so a pre-stream
    // failure never delivers a partial `start` with no terminal. `run_mapper`
    // applies the model-aware `cost_fn` on the final usage before the terminal
    // event (mirrors the inline `calculateCost(model, output.usage)` calls).
    let mut sse = SseEventStream::new(response, opts.signal.clone())
        .with_idle_timeout(opts.stream_idle_timeout())
        .with_diagnostics(diagnostics.enabled);
    let cost_model = model.cost.clone();
    let cost_fn = move |usage: &Usage| calculate_cost(&cost_model, usage);
    let mapper_started = diagnostics.enabled.then(Instant::now);
    if diagnostics.enabled {
        let mut mapper_diagnostics = MapperDiagnostics::default();
        run_mapper_with_diagnostics(&mut sse, prod, &mut state, cost_fn, &mut mapper_diagnostics)
            .await;
        diagnostics.mapper_ns = ProviderRequestDiagnostics::elapsed(mapper_started);
        diagnostics.sse_events = mapper_diagnostics.sse_events;
        diagnostics.mapper_pull_ns = mapper_diagnostics.pull_ns;
        diagnostics.mapper_parse_ns = mapper_diagnostics.parse_ns;
        diagnostics.mapper_apply_ns = mapper_diagnostics.apply_ns;
    } else {
        run_mapper(&mut sse, prod, &mut state, cost_fn).await;
    }
    diagnostics.sse_first_event_ns = sse.diagnostics_first_event_ns().unwrap_or_default();
}

fn strip_cache_control(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.remove("cache_control");
            for child in map.values_mut() {
                strip_cache_control(child);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                strip_cache_control(child);
            }
        }
        _ => {}
    }
}

fn simplify_non_stream_request(value: &mut serde_json::Value) {
    let text_from_blocks = |blocks: &serde_json::Value| {
        blocks
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("text").and_then(serde_json::Value::as_str))
                    .collect::<String>()
            })
            .unwrap_or_default()
    };
    if let Some(messages) = value
        .get_mut("messages")
        .and_then(serde_json::Value::as_array_mut)
    {
        for message in messages {
            if let Some(content) = message.get_mut("content") {
                if content.is_array() {
                    *content = serde_json::Value::String(text_from_blocks(content));
                }
            }
        }
    }
    if let Some(system) = value.get_mut("system") {
        if system.is_array() {
            *system = serde_json::Value::String(text_from_blocks(system));
        }
    }
}

// ----------------------------------------------------------------------------
// Header + auth helpers — mirrors createClient (API-key arm) + assertRequestAuth.
// ----------------------------------------------------------------------------

/// Resolve the API key: `opts.api_key` wins, then the provider default, then
/// `ANTHROPIC_API_KEY` from the environment. Mirrors the TS fallback chain
/// (`options?.apiKey` → SDK's `apiKey: null` reads the env var internally).
fn resolve_api_key(
    provider_key: &Option<String>,
    opts: &SimpleStreamOptions,
    allow_env_api_key: bool,
) -> Option<String> {
    resolve_api_key_with_env(provider_key, opts, allow_env_api_key, || {
        std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
    })
}

fn resolve_api_key_with_env(
    provider_key: &Option<String>,
    opts: &SimpleStreamOptions,
    allow_env_api_key: bool,
    read_env_api_key: impl FnOnce() -> Option<String>,
) -> Option<String> {
    opts.api_key
        .clone()
        .or_else(|| provider_key.clone())
        .or_else(|| allow_env_api_key.then(read_env_api_key).flatten())
}

/// True when `headers` carries an `authorization`, `x-api-key`, or
/// `cf-aig-authorization` header — the auth-owned-header cases
/// `assertRequestAuth` accepts without a resolved API key. Mirrors `hasHeader`
/// over those three names (case-insensitive).
fn has_header_auth(headers: &Option<std::collections::BTreeMap<String, String>>) -> bool {
    let Some(h) = headers else {
        return false;
    };
    const NAMES: &[&str] = &["authorization", "x-api-key", "cf-aig-authorization"];
    h.keys()
        .any(|k| NAMES.contains(&k.to_ascii_lowercase().as_str()))
}

/// Assemble the HTTP header set for the POST. Mirrors the API-key arm of TS
/// `createClient` (anthropic-messages.ts:914-936): `accept`,
/// `anthropic-dangerous-direct-browser-access`, `anthropic-version`,
/// `anthropic-beta` (when present), `x-session-affinity` (when enabled), then
/// `model.headers` and `opts.headers` merged (later wins). `x-api-key` is
/// applied LAST so caller headers cannot clobber the resolved key (the SDK
/// attaches it out-of-band from `defaultHeaders`).
fn assemble_headers(
    model: &Model,
    opts: &SimpleStreamOptions,
    beta_header: Option<&str>,
    api_key: Option<&str>,
) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = Vec::new();
    headers.push(("accept".into(), "application/json".into()));
    // Keep the request identity aligned with the working third-party
    // Anthropic clients (including matrixcode). Some gateways use this
    // identity when classifying requests from coding-agent clients.
    headers.push(("user-agent".into(), "curl/8.0".into()));
    if std::env::var("RPI_ANTHROPIC_NON_STREAM").ok().as_deref() != Some("1") {
        headers.push((
            "anthropic-dangerous-direct-browser-access".into(),
            "true".into(),
        ));
    }
    headers.push(("anthropic-version".into(), ANTHROPIC_VERSION.into()));
    if let Some(beta) = beta_header {
        headers.push(("anthropic-beta".into(), beta.into()));
    }

    // Session affinity: TS `cacheSessionId = cacheRetention === "none" ?
    // undefined : options?.sessionId`, then gated on the model's
    // `sendSessionAffinityHeaders` compat flag (default false).
    if !matches!(opts.cache_retention, CacheRetention::None) {
        if let Some(session_id) = &opts.session_id {
            let send = model
                .compat
                .as_ref()
                .and_then(|c| match c {
                    StreamingProtocolCompat::AnthropicMessages(a) => Some(a),
                    _ => None,
                })
                .map(|a| a.send_session_affinity_headers.unwrap_or(false))
                .unwrap_or(false);
            if send {
                headers.push(("x-session-affinity".into(), session_id.clone()));
            }
        }
    }

    // model.headers then opts.headers (later wins). Mirrors `mergeHeaders`.
    if let Some(model_headers) = &model.headers {
        for (k, v) in model_headers {
            merge_header(&mut headers, k.clone(), v.clone());
        }
    }
    if let Some(opt_headers) = &opts.headers {
        for (k, v) in opt_headers {
            merge_header(&mut headers, k.clone(), v.clone());
        }
    }

    // x-api-key applied last so it is not overridable by model/opts headers
    // (matches the SDK attaching it out-of-band). When a key is resolved it is
    // authoritative: drop any `x-api-key` a caller injected via `headers` so
    // only the resolved key is sent. Header-owned-auth callers pass
    // `api_key = None` here and rely on their own `x-api-key` / `authorization`
    // header surviving the merges above.
    let oauth_key = api_key.filter(|key| key.starts_with("sk-ant-oat"));
    if let Some(key) = oauth_key {
        headers.retain(|(name, _)| !name.eq_ignore_ascii_case("x-api-key"));
        merge_header(
            &mut headers,
            "authorization".into(),
            format!("Bearer {key}"),
        );
    } else if let Some(key) = api_key.filter(|_| is_vertex_anthropic(model)) {
        headers.retain(|(name, _)| !name.eq_ignore_ascii_case("x-api-key"));
        merge_header(
            &mut headers,
            "authorization".into(),
            format!("Bearer {key}"),
        );
    } else if let Some(key) = api_key {
        headers.retain(|(k, _)| k.to_ascii_lowercase() != "x-api-key");
        headers.push(("x-api-key".into(), key.into()));
    }
    let is_oauth = oauth_key.is_some()
        || headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("authorization") && value.starts_with("Bearer sk-ant-oat")
        });
    if is_oauth {
        let existing = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        let mut betas: Vec<&str> = existing
            .split(',')
            .filter(|value| !value.is_empty())
            .collect();
        for beta in [
            "oauth-2025-04-20",
            "claude-code-20250219",
            "interleaved-thinking-2025-05-14",
        ] {
            if !betas.contains(&beta) {
                betas.push(beta);
            }
        }
        merge_header(&mut headers, "anthropic-beta".into(), betas.join(","));
        merge_header(
            &mut headers,
            "user-agent".into(),
            "claude-cli/2.1.321 (external, cli)".into(),
        );
        merge_header(&mut headers, "x-app".into(), "cli".into());
        if let Some(session) = &opts.session_id {
            merge_header(
                &mut headers,
                "x-claude-code-session-id".into(),
                session.clone(),
            );
        }
    }

    headers
}

/// Insert-or-replace a header by lowercase name. Mirrors TS `mergeHeaders` /
/// `Object.assign` last-wins semantics.
fn merge_header(headers: &mut Vec<(String, String)>, name: String, value: String) {
    let lname = name.to_ascii_lowercase();
    if let Some(slot) = headers
        .iter_mut()
        .find(|(k, _)| k.to_ascii_lowercase() == lname)
    {
        slot.1 = value;
    } else {
        headers.push((name, value));
    }
}

/// Wall-clock ms-since-epoch for the assistant message timestamp. Mirrors TS
/// `Date.now()`. The provider is the one `pi-ai` component that touches real
/// time: the message timestamp is persisted into the harness JSONL log, so a
/// monotonic counter (as faux uses) would be wrong here.
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::anthropic::models::claude_haiku_4_5;
    use crate::types::{Api, InputModality, ModelCost};
    use std::collections::BTreeMap;

    /// Build a minimal model for header/auth tests (no network).
    fn test_model() -> Model {
        let mut m = Model::new(
            "claude-haiku-4-5",
            "Claude Haiku 4.5",
            Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        m.input = vec![InputModality::Text];
        m.context_window = 200_000;
        m.max_tokens = 8_192;
        m.cost = ModelCost::default();
        m.compat = Some(crate::model::StreamingProtocolCompat::AnthropicMessages(
            crate::model::AnthropicMessagesCompat::default(),
        ));
        m
    }

    #[test]
    fn assemble_headers_includes_required_defaults() {
        let model = test_model();
        let opts = SimpleStreamOptions::default();
        let headers = assemble_headers(&model, &opts, None, Some("sk-test"));
        let names: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"accept"));
        assert!(names.contains(&"user-agent"));
        assert!(names.contains(&"anthropic-version"));
        assert!(names.contains(&"anthropic-dangerous-direct-browser-access"));
        assert!(names.contains(&"x-api-key"));
        // No beta header when none requested.
        assert!(!names.contains(&"anthropic-beta"));
        let version = headers
            .iter()
            .find(|(k, _)| k == "anthropic-version")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(version, ANTHROPIC_VERSION);
        let user_agent = headers
            .iter()
            .find(|(k, _)| k == "user-agent")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(user_agent, "curl/8.0");
    }

    #[test]
    fn assemble_headers_applies_beta_when_present() {
        let model = test_model();
        let opts = SimpleStreamOptions::default();
        let headers = assemble_headers(
            &model,
            &opts,
            Some("fine-grained-tool-streaming-2025-05-14"),
            Some("k"),
        );
        let beta = headers
            .iter()
            .find(|(k, _)| k == "anthropic-beta")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(beta, "fine-grained-tool-streaming-2025-05-14");
    }

    #[test]
    fn assemble_headers_x_api_key_not_overridable_by_opts() {
        let model = test_model();
        let mut opts = SimpleStreamOptions::default();
        let mut extra = std::collections::BTreeMap::new();
        // Caller tries to override the resolved key via opts.headers — should
        // be ignored (x-api-key applied last wins), matching SDK semantics.
        extra.insert("x-api-key".to_string(), "SK-ATTACK".to_string());
        opts.headers = Some(extra);
        let headers = assemble_headers(&model, &opts, None, Some("sk-resolved"));
        let key = headers
            .iter()
            .find(|(k, _)| k == "x-api-key")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(key, "sk-resolved");
    }

    #[test]
    fn assemble_headers_session_affinity_gated_on_compat_and_retention() {
        // Haiku 4.5 uses the catalog model which has send_session_affinity off
        // (default false) → no x-session-affinity even with a session id.
        let mut model = claude_haiku_4_5();
        let _ = &mut model; // catalog model, compat default has no affinity flag
        let mut opts = SimpleStreamOptions::default();
        opts.session_id = Some("sess-1".into());
        opts.cache_retention = CacheRetention::Short;
        let headers = assemble_headers(&model, &opts, None, Some("k"));
        assert!(
            !headers.iter().any(|(k, _)| k == "x-session-affinity"),
            "default catalog should not send session affinity"
        );

        // Opt in via compat flag → header emitted.
        let mut model = test_model();
        let mut compat = crate::model::AnthropicMessagesCompat::default();
        compat.send_session_affinity_headers = Some(true);
        model.compat = Some(StreamingProtocolCompat::AnthropicMessages(compat));
        let headers = assemble_headers(&model, &opts, None, Some("k"));
        assert_eq!(
            headers
                .iter()
                .find(|(k, _)| k == "x-session-affinity")
                .map(|(_, v)| v.as_str()),
            Some("sess-1")
        );

        // CacheRetention::None suppresses it even when compat is on.
        let mut opts2 = opts.clone();
        opts2.cache_retention = CacheRetention::None;
        let headers = assemble_headers(&model, &opts2, None, Some("k"));
        assert!(
            !headers.iter().any(|(k, _)| k == "x-session-affinity"),
            "None retention suppresses session affinity"
        );
    }

    #[test]
    fn assemble_headers_model_and_opts_merge_last_wins() {
        let mut model = test_model();
        let mut mh = std::collections::BTreeMap::new();
        mh.insert("x-custom".to_string(), "from-model".to_string());
        model.headers = Some(mh);
        let mut opts = SimpleStreamOptions::default();
        let mut oh = std::collections::BTreeMap::new();
        oh.insert("x-custom".to_string(), "from-opts".to_string());
        oh.insert("x-extra".to_string(), "extra".to_string());
        opts.headers = Some(oh);
        let headers = assemble_headers(&model, &opts, None, None);
        assert_eq!(
            headers
                .iter()
                .find(|(k, _)| k == "x-custom")
                .map(|(_, v)| v.as_str()),
            Some("from-opts"),
            "opts.headers wins over model.headers"
        );
        assert_eq!(
            headers
                .iter()
                .find(|(k, _)| k == "x-extra")
                .map(|(_, v)| v.as_str()),
            Some("extra")
        );
    }

    #[test]
    fn has_header_auth_detects_owned_auth_headers() {
        let mut h = std::collections::BTreeMap::new();
        h.insert("X-API-Key".to_string(), "k".to_string());
        assert!(has_header_auth(&Some(h.clone())));
        let mut h2 = std::collections::BTreeMap::new();
        h2.insert("authorization".to_string(), "Bearer t".to_string());
        assert!(has_header_auth(&Some(h2)));
        assert!(!has_header_auth(&None));
        let mut h3 = std::collections::BTreeMap::new();
        h3.insert("x-other".to_string(), "v".to_string());
        assert!(!has_header_auth(&Some(h3)));
    }

    #[test]
    fn resolve_api_key_prefers_opts_then_provider_then_env() {
        let opts = SimpleStreamOptions::default().with_api_key("opts-key");
        assert_eq!(
            resolve_api_key(&Some("provider-key".into()), &opts, true),
            Some("opts-key".into())
        );

        let opts = SimpleStreamOptions::default();
        assert_eq!(
            resolve_api_key(&Some("provider-key".into()), &opts, true),
            Some("provider-key".into())
        );
    }

    #[test]
    fn isolated_custom_provider_never_adds_anthropic_env_key_to_headers() {
        let mut model = test_model();
        model.provider = "gateway".into();
        model.base_url = "https://gateway.example.com".into();
        model.headers = Some(BTreeMap::from([(
            "authorization".into(),
            "Bearer gateway-key".into(),
        )]));
        let opts = SimpleStreamOptions::default();
        let env_read = std::cell::Cell::new(false);

        let resolved = resolve_api_key_with_env(&None, &opts, false, || {
            env_read.set(true);
            Some("official-anthropic-key".into())
        });
        let headers = assemble_headers(&model, &opts, None, resolved.as_deref());

        assert!(
            !env_read.get(),
            "isolated providers must not read Anthropic env auth"
        );
        assert!(
            headers.iter().all(|(name, value)| {
                !name.eq_ignore_ascii_case("x-api-key") && value != "official-anthropic-key"
            }),
            "the official key must never reach a custom provider request"
        );
        assert_eq!(
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .map(|(_, value)| value.as_str()),
            Some("Bearer gateway-key")
        );
    }

    #[test]
    fn provider_exposes_catalog_models() {
        let p = AnthropicProvider::from_env();
        assert_eq!(p.id(), "anthropic");
        assert!(!p.models().is_empty());
        assert!(p.models().iter().any(|m| m.id == "claude-haiku-4-5"));
        assert!(p.api_key().is_none(), "from_env does not pre-read the key");
    }

    /// Regression for the Bearer-on-`model.headers` path (the
    /// `ANTHROPIC_AUTH_TOKEN` / `models.json authHeader:true` routes in
    /// rpi-cli's `provider::resolve`): when the auth header lives on
    /// `model.headers` and no key is set, the header-owned-auth check must pass
    /// so the request is NOT rejected as "No API key for provider". This mirrors
    /// the net effect of upstream `assertRequestAuth`, which runs *after* the
    /// model-resolution layer has merged `model.headers` into `options.headers`.
    #[test]
    fn header_auth_on_model_headers_counts_as_owned() {
        let mut model = test_model();
        let mut mh = std::collections::BTreeMap::new();
        mh.insert("authorization".to_string(), "Bearer tok".to_string());
        model.headers = Some(mh);

        // No opts.headers, no key — the only auth is on the model. Must satisfy.
        let owned = has_header_auth(&SimpleStreamOptions::default().headers)
            || has_header_auth(&model.headers);
        assert!(
            owned,
            "a Bearer header on model.headers must satisfy header-owned auth"
        );

        // A model carrying only non-auth headers must NOT satisfy it.
        let mut model2 = test_model();
        let mut mh2 = std::collections::BTreeMap::new();
        mh2.insert("x-custom".to_string(), "v".to_string());
        model2.headers = Some(mh2);
        let owned2 = has_header_auth(&SimpleStreamOptions::default().headers)
            || has_header_auth(&model2.headers);
        assert!(
            !owned2,
            "non-auth headers on model.headers must not satisfy header-owned auth"
        );
    }
}

#[cfg(test)]
mod oauth_header_tests {
    use super::*;

    #[test]
    fn subscription_oauth_uses_bearer_and_retains_feature_betas() {
        let model = Model::new(
            "claude-test",
            "Claude",
            crate::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let headers = assemble_headers(
            &model,
            &SimpleStreamOptions::default(),
            Some("prompt-caching-2024-07-31"),
            Some("sk-ant-oat-test"),
        );
        let value = |name: &str| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(value("authorization"), Some("Bearer sk-ant-oat-test"));
        assert_eq!(value("x-api-key"), None);
        assert!(value("anthropic-beta")
            .unwrap()
            .contains("oauth-2025-04-20"));
        assert!(value("anthropic-beta")
            .unwrap()
            .contains("prompt-caching-2024-07-31"));
        assert_eq!(value("x-app"), Some("cli"));
    }
}

fn anthropic_messages_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/messages") {
        base.to_owned()
    } else if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

#[cfg(test)]
mod endpoint_tests {
    use super::anthropic_messages_url;

    #[test]
    fn messages_endpoint_preserves_native_gateway_version_prefixes() {
        assert_eq!(
            anthropic_messages_url("https://api.anthropic.com"),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://api.kimi.com/coding/v1"),
            "https://api.kimi.com/coding/v1/messages"
        );
        assert_eq!(
            anthropic_messages_url("https://proxy.test/custom/v1/messages/"),
            "https://proxy.test/custom/v1/messages"
        );
    }
}

fn is_vertex_anthropic(model: &Model) -> bool {
    model.provider == "google-vertex" && model.base_url.contains("/publishers/anthropic/models/")
}

fn vertex_anthropic_url(model: &Model, options: &SimpleStreamOptions) -> Result<String, String> {
    let header = |name: &str| {
        options
            .headers
            .as_ref()
            .and_then(|headers| {
                headers
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value.clone())
            })
            .or_else(|| {
                model.headers.as_ref().and_then(|headers| {
                    headers
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case(name))
                        .map(|(_, value)| value.clone())
                })
            })
    };
    let mut url = model.base_url.clone();
    if url.contains("{project}") {
        let project = header("x-omp-google-project")
            .or_else(|| std::env::var("GOOGLE_CLOUD_PROJECT").ok())
            .or_else(|| std::env::var("GCLOUD_PROJECT").ok())
            .ok_or("Google Vertex resource project is required")?;
        url = url.replace("{project}", &project);
    }
    if url.contains("{location}") {
        let location = header("x-goog-location")
            .or_else(|| std::env::var("GOOGLE_CLOUD_LOCATION").ok())
            .unwrap_or_else(|| "global".into());
        url = url.replace("{location}", &location);
        if location == "global" {
            url = url.replace(
                "global-aiplatform.googleapis.com",
                "aiplatform.googleapis.com",
            );
        }
    }
    if !url.ends_with(":streamRawPredict") && !url.ends_with(":rawPredict") {
        return Err(
            "Vertex Anthropic endpoint must end in :streamRawPredict or :rawPredict".into(),
        );
    }
    Ok(url)
}

fn shape_vertex_anthropic_body(body: &mut serde_json::Value, betas: Option<&str>) {
    if let Some(object) = body.as_object_mut() {
        object.remove("model");
        object.insert(
            "anthropic_version".into(),
            serde_json::json!("vertex-2023-10-16"),
        );
        if let Some(betas) = betas {
            object.insert(
                "anthropic_beta".into(),
                serde_json::json!(betas
                    .split(',')
                    .filter(|beta| !beta.is_empty())
                    .collect::<Vec<_>>()),
            );
        }
    }
}

#[cfg(test)]
mod vertex_tests {
    use super::*;
    #[test]
    fn vertex_anthropic_uses_publisher_endpoint_body_and_resource_project() {
        let model = Model::new("claude-test", "Claude", crate::Api::AnthropicMessages, "google-vertex", "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/publishers/anthropic/models/claude-test:streamRawPredict");
        let options = SimpleStreamOptions {
            headers: Some(std::collections::BTreeMap::from([
                ("x-omp-google-project".into(), "resource-project".into()),
                ("x-goog-user-project".into(), "quota-project".into()),
                ("x-goog-location".into(), "global".into()),
            ])),
            ..Default::default()
        };
        assert_eq!(vertex_anthropic_url(&model, &options).unwrap(), "https://aiplatform.googleapis.com/v1/projects/resource-project/locations/global/publishers/anthropic/models/claude-test:streamRawPredict");
        let mut body = serde_json::json!({"model":"claude-test","stream":true,"messages":[{"role":"user","content":"Hi"}]});
        shape_vertex_anthropic_body(&mut body, Some("feature-one,feature-two"));
        assert!(body.get("model").is_none());
        assert_eq!(body["anthropic_version"], "vertex-2023-10-16");
        assert_eq!(
            body["anthropic_beta"],
            serde_json::json!(["feature-one", "feature-two"])
        );
        assert_eq!(body["stream"], true);
    }
}

#[cfg(test)]
mod anonymous_gateway_tests {
    use super::*;
    #[test]
    fn anonymous_auth_requires_explicit_transport_opt_in() {
        let model = Model::new(
            "claude-test",
            "Claude",
            crate::Api::AnthropicMessages,
            "gateway",
            "https://gateway.test",
        );
        assert!(prepare_params(
            &model,
            &Context::default(),
            &SimpleStreamOptions::default(),
            &None,
            false,
            true
        )
        .is_err());
        let prepared = prepare_params(
            &model,
            &Context::default(),
            &SimpleStreamOptions::default(),
            &None,
            false,
            false,
        )
        .unwrap();
        assert!(prepared.api_key_for_header.is_none());
    }
}
