//! Mirrors `packages/ai/src/models.ts::Provider` — the trait every concrete
//! provider (faux, anthropic, …) implements. The agent loop never touches a
//! provider's wire format directly; it calls `stream_simple` and consumes the
//! returned [`AssistantMessageEventStream`].

use crate::event_stream::AssistantMessageEventStream;
use crate::model::Model;
use crate::redact::RedactedHeaders;
use crate::types::{AssistantMessage, Context, DeferredHandle, ThinkingBudgets, ThinkingLevel};
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Default deadline for one LLM API request, including its streamed response.
pub const DEFAULT_LLM_API_TIMEOUT: Duration = Duration::from_secs(600);

/// Default maximum silence between response body bytes on an active stream.
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Cache-retention hint passed to a provider. Mirrors TS `CacheRetention`.
/// `Long` requests 1h ephemeral cache control on Anthropic (subject to compat);
/// `Short` requests the default ephemeral; `None` disables cache control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheRetention {
    None,
    #[default]
    Short,
    Long,
}

/// Options for a "simple" (non-agent) stream call — what compaction / smoke
/// tests / bare provider usage pass. The agent loop wraps this with its richer
/// equivalent when it drives a real turn (M2).
#[derive(Clone)]
pub struct SimpleStreamOptions {
    /// API key. A provider may fall back to an env var when `None` (M3 anthropic
    /// reads `ANTHROPIC_API_KEY`); faux ignores it.
    pub api_key: Option<String>,
    /// Request timeout. Defaults to 600 seconds; `None` uses the same provider
    /// fallback and can be used by patches to clear a caller override.
    pub timeout: Option<Duration>,
    /// Maximum silence between streamed body bytes. `None` uses the 300-second
    /// default; `Some(Duration::ZERO)` disables it. Independent of `timeout`.
    pub stream_idle_timeout: Option<Duration>,
    /// Max retry attempts on retryable errors (408/409/429/>=500). `None` = 0.
    pub max_retries: Option<u32>,
    /// Cap on the total backoff between retries. `None` = 60s.
    pub max_retry_delay: Option<Duration>,
    /// Extra HTTP headers to send (merged over provider defaults by
    /// case-insensitive header name).
    pub headers: Option<BTreeMap<String, String>>,
    /// Free-form provider metadata (request id, trace id, …). Anthropic reads
    /// `user_id` for abuse tracking; faux ignores it.
    pub metadata: Option<BTreeMap<String, String>>,
    /// Cache-retention hint for prompt caching.
    pub cache_retention: CacheRetention,
    /// Optional session id (faux uses it to simulate prompt-cache hits).
    pub session_id: Option<String>,
    /// Cancellation token. Providers must poll this between chunks and emit an
    /// `Error { reason: Aborted }` terminal event when cancelled. Defaults to a
    /// fresh un-cancelled token so `Default`-constructed options never abort.
    pub signal: CancellationToken,
    /// Extended-thinking level. `None`/`Off` → thinking disabled (provider may
    /// still emit `thinking: { type: "disabled" }` when the model reasons).
    /// Mirrors TS `SimpleStreamOptions.reasoning`.
    pub reasoning: Option<ThinkingLevel>,
    /// Optional caller override for the requested output-token ceiling. `None`
    /// lets the provider fit the thinking budget inside `Model::max_tokens`.
    /// Mirrors TS `SimpleStreamOptions.maxTokens`.
    pub max_tokens: Option<u64>,
    /// Sampling temperature. Only applied when the model supports it AND
    /// thinking is disabled (Anthropic rejects temperature under thinking).
    /// Mirrors TS `StreamOptions.temperature`.
    pub temperature: Option<f64>,
    /// Per-level reasoning token budgets for budget-based thinking models.
    /// Faux ignores this; anthropic uses it in `adjust_max_tokens_for_thinking`.
    pub thinking_budgets: Option<ThinkingBudgets>,
}

impl Default for SimpleStreamOptions {
    fn default() -> Self {
        Self {
            api_key: None,
            timeout: Some(DEFAULT_LLM_API_TIMEOUT),
            stream_idle_timeout: Some(DEFAULT_STREAM_IDLE_TIMEOUT),
            max_retries: None,
            max_retry_delay: None,
            headers: None,
            metadata: None,
            cache_retention: CacheRetention::default(),
            session_id: None,
            signal: CancellationToken::new(),
            reasoning: None,
            max_tokens: None,
            temperature: None,
            thinking_budgets: None,
        }
    }
}

/// `Debug` redacts credentials: `api_key` is shown only as present or absent,
/// and sensitive header values are masked. The destructuring below is
/// exhaustive, so adding a field to the struct fails to compile until it is
/// printed here too.
impl fmt::Debug for SimpleStreamOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            api_key,
            timeout,
            stream_idle_timeout,
            max_retries,
            max_retry_delay,
            headers,
            metadata,
            cache_retention,
            session_id,
            signal,
            reasoning,
            max_tokens,
            temperature,
            thinking_budgets,
        } = self;
        f.debug_struct("SimpleStreamOptions")
            .field("api_key", &api_key.as_ref().map(|_| "<redacted>"))
            .field("timeout", timeout)
            .field("stream_idle_timeout", stream_idle_timeout)
            .field("max_retries", max_retries)
            .field("max_retry_delay", max_retry_delay)
            .field("headers", &headers.as_ref().map(RedactedHeaders))
            .field("metadata", metadata)
            .field("cache_retention", cache_retention)
            .field("session_id", session_id)
            .field("signal", signal)
            .field("reasoning", reasoning)
            .field("max_tokens", max_tokens)
            .field("temperature", temperature)
            .field("thinking_budgets", thinking_budgets)
            .finish()
    }
}

impl SimpleStreamOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    pub fn with_signal(mut self, signal: CancellationToken) -> Self {
        self.signal = signal;
        self
    }

    /// Resolve the effective reasoning level, defaulting to `Off` when unset.
    pub fn reasoning_level(&self) -> ThinkingLevel {
        self.reasoning.unwrap_or(ThinkingLevel::Off)
    }

    /// Resolve the effective request timeout, including the provider fallback.
    pub fn request_timeout(&self) -> Duration {
        self.timeout.unwrap_or(DEFAULT_LLM_API_TIMEOUT)
    }

    /// Effective stream idle timeout; zero disables the watchdog.
    pub fn stream_idle_timeout(&self) -> Option<Duration> {
        let duration = self
            .stream_idle_timeout
            .unwrap_or(DEFAULT_STREAM_IDLE_TIMEOUT);
        (!duration.is_zero()).then_some(duration)
    }
}

/// Replace or remove every spelling of an HTTP header name. Header values and
/// the selected spelling are preserved; metadata keys remain case-sensitive.
pub fn set_http_header(headers: &mut BTreeMap<String, String>, name: &str, value: Option<&str>) {
    headers.retain(|existing, _| !existing.eq_ignore_ascii_case(name));
    if let Some(value) = value {
        headers.insert(name.to_owned(), value.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::{SimpleStreamOptions, DEFAULT_LLM_API_TIMEOUT};
    use std::time::Duration;

    #[test]
    fn header_patches_replace_and_delete_names_without_case_sensitivity() {
        let mut options = super::SimpleStreamOptions {
            headers: Some(std::collections::BTreeMap::from([
                ("Authorization".into(), "Bearer Old".into()),
                ("X-Tenant".into(), "tenant".into()),
            ])),
            ..Default::default()
        };
        super::SimpleStreamOptionsPatch {
            headers: Some(std::collections::BTreeMap::from([
                ("authorization".into(), Some("Bearer New".into())),
                ("x-tenant".into(), None),
            ])),
            ..Default::default()
        }
        .apply(&mut options);
        assert_eq!(
            options.headers,
            Some(std::collections::BTreeMap::from([(
                "authorization".into(),
                "Bearer New".into()
            )]))
        );
    }

    #[test]
    fn llm_api_timeout_defaults_to_ten_minutes() {
        let options = SimpleStreamOptions::default();
        assert_eq!(options.timeout, Some(Duration::from_secs(600)));
        assert_eq!(options.request_timeout(), DEFAULT_LLM_API_TIMEOUT);
    }

    #[test]
    fn stream_idle_timeout_is_independent_and_can_be_disabled_or_reset() {
        let mut options = SimpleStreamOptions::default();
        assert_eq!(
            options.stream_idle_timeout(),
            Some(Duration::from_secs(300))
        );
        super::SimpleStreamOptionsPatch {
            stream_idle_timeout: Some(Some(Duration::ZERO)),
            ..Default::default()
        }
        .apply(&mut options);
        assert_eq!(options.stream_idle_timeout(), None);
        assert_eq!(options.request_timeout(), Duration::from_secs(600));
        super::SimpleStreamOptionsPatch {
            stream_idle_timeout: Some(None),
            ..Default::default()
        }
        .apply(&mut options);
        assert_eq!(
            options.stream_idle_timeout(),
            Some(Duration::from_secs(300))
        );
    }

    #[test]
    fn debug_output_redacts_api_key_and_sensitive_headers() {
        let options = SimpleStreamOptions {
            api_key: Some("sk-live-SECRET-KEY".into()),
            headers: Some(std::collections::BTreeMap::from([
                ("Authorization".into(), "Bearer SECRET-BEARER".into()),
                ("x-api-key".into(), "SECRET-XAPIKEY".into()),
                ("Proxy-Authorization".into(), "SECRET-PROXY".into()),
                ("Cookie".into(), "SECRET-COOKIE".into()),
                ("x-session-token".into(), "SECRET-TOKEN".into()),
                ("x-client-secret".into(), "SECRET-CLIENT".into()),
                ("X-Tenant".into(), "tenant-visible".into()),
            ])),
            max_retries: Some(3),
            ..Default::default()
        };
        let debug = format!("{options:?}");
        for secret in [
            "SECRET-KEY",
            "SECRET-BEARER",
            "SECRET-XAPIKEY",
            "SECRET-PROXY",
            "SECRET-COOKIE",
            "SECRET-TOKEN",
            "SECRET-CLIENT",
        ] {
            assert!(!debug.contains(secret), "leaked {secret} in {debug}");
        }
        assert!(debug.contains("api_key: Some(\"<redacted>\")"), "{debug}");
        assert!(
            debug.contains("\"X-Tenant\": \"tenant-visible\""),
            "{debug}"
        );
        assert!(debug.contains("max_retries: Some(3)"), "{debug}");
        assert!(debug.contains("stream_idle_timeout"), "{debug}");
    }

    #[test]
    fn llm_api_timeout_allows_override_and_none_uses_provider_fallback() {
        let mut options = SimpleStreamOptions::default();
        options.timeout = Some(Duration::from_secs(15));
        assert_eq!(options.request_timeout(), Duration::from_secs(15));

        options.timeout = None;
        assert_eq!(options.request_timeout(), Duration::from_secs(600));
    }
}

/// The trait seam. The arrow `pi-agent → pi-ai` means the agent loop depends on
/// this trait alone — provider crates are pluggable behind it.
///
/// `stream_simple` returns synchronously (no `await`): it creates an
/// [`AssistantMessageEventStream`], spawns the producer task internally, and
/// hands back the consumer. Failures surface as `Error` events on the stream,
/// never as panics or `Err` from this method.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// Stable provider id (e.g. `"anthropic"`, `"faux"`).
    fn id(&self) -> &str;

    /// Models this provider serves.
    fn models(&self) -> &[Model];

    /// Stream a single assistant turn against `ctx` on `model`. Returns the
    /// consumer end of an event stream; the producer task is already running.
    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream;

    /// This provider's deferred-response capability, if it has one.
    ///
    /// Defaulted rather than required, and returned as an optional sidecar:
    /// only providers whose API actually has a long-poll continuation can
    /// answer, and the rest should not carry a stub. This is the same reasoning
    /// that put [`ProviderHooks`] in its own trait, except a default body also
    /// spares every existing implementation.
    fn deferred(&self) -> Option<&dyn DeferredProvider> {
        None
    }
}

/// Continue a response the provider deferred (a long-poll or batch API).
///
/// Mirrors the optional `streamDeferred` / `cancelDeferred` on upstream's models
/// interface. Same contract as [`Provider::stream_simple`]: returns the consumer
/// end of a stream and reports failures as `Error` events rather than `Err`, so
/// the harness handles both kinds of provider call uniformly.
#[async_trait::async_trait]
pub trait DeferredProvider: Send + Sync {
    /// Poll a handle this provider previously issued: either the finished
    /// assistant message, or another `Deferred` handle to poll again.
    async fn stream_deferred(
        &self,
        model: &Model,
        handle: &DeferredHandle,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream;

    /// Best-effort remote cancellation. Defaulting to `Ok` is right because most
    /// long-poll APIs cannot be cancelled, and a caller must never be blocked by
    /// that.
    async fn cancel_deferred(
        &self,
        _model: &Model,
        _handle: &DeferredHandle,
    ) -> Result<(), String> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ProviderHooks (B4) — a per-call sidecar hook fired inside the harness
// `StreamFn` closure BEFORE each `stream_simple` call.
//
// Why a sidecar trait (not an extension of `Provider`): `Provider` is not
// sealed (verified), so extending it directly is viable — but that would force
// EVERY `Provider` impl to stub no-op hooks, even faux/anthropic. A sidecar
// trait keeps `Provider` lean and lets only the host (harness) supply hooks;
// the harness captures `Option<Arc<dyn ProviderHooks>>` into its `StreamFn`
// closure and applies the patch per call. This mirrors pi's `beforeRequest`
// per-call semantics, NOT a run-once config-build patch (applying once per run
// is not equivalent — e.g. a hook that injects a per-request id or rotates a
// header must run before every provider call).
//
// The patch type is `SimpleStreamOptionsPatch` (against `SimpleStreamOptions`,
// the type actually passed to `stream_simple`) — NOT the harness-side
// `AgentHarnessStreamOptionsPatch` (which targets `AgentHarnessStreamOptions`,
// the pinned subset, and lives in rpi-harness): defining it here keeps rpi-ai
// free of any rpi-harness dep (cycle-free) and targets the live options the
// provider sees. The harness's existing `AgentHarnessStreamOptionsPatch` stays
// for harness config, a separate concern.
// ---------------------------------------------------------------------------

/// A per-call provider hook. `before_request` may patch the live
/// [`SimpleStreamOptions`] (e.g. inject/rotate headers, set metadata, override
/// timeout) before the harness calls `stream_simple`; `after_response` observes
/// the terminal assistant message after the stream resolves. Both are
/// `Option`-returning: `None` = no change / no observation.
///
/// Implementations MUST be infallible at the API boundary — a hook error is
/// swallowed (the run continues with the unpatched options), mirroring the
/// "hooks must not throw" contract from pi and the harness's other hooks
/// (`before_tool_call` etc.).
pub trait ProviderHooks: Send + Sync {
    /// Fired before each `stream_simple` call, with a snapshot of the model +
    /// context + opts the harness is about to pass. Return a patch that the
    /// harness applies to a clone of `ctx` before calling the provider; return
    /// `None` to leave `ctx` untouched.
    ///
    /// This is the seam pi calls `transform_context`
    /// (`harness/execution/assistant.ts:142`), and it fires on **every** provider
    /// request of a run — the same cadence as [`Self::before_request`], and the
    /// reason a plugin can change the prompt for one turn without the change
    /// sticking to the session. It runs before `before_request` so a prompt the
    /// hook installs is what that request carries.
    ///
    /// Unlike `before_request` this cannot patch `opts`; the two are separate so
    /// a caller can reason about "what the model sees" independently of "how the
    /// request is sent".
    ///
    /// Must not retain the borrow. Runs on the blocking-bridge thread inside the
    /// harness `StreamFn` closure.
    fn transform_context(&self, _model: &Model, _ctx: &Context) -> Option<ContextPatch> {
        None
    }

    /// Fired before each `stream_simple` call, with a snapshot of the model +
    /// context + opts the harness is about to pass. Return a patch that the
    /// harness applies to a clone of `opts` before calling the provider; return
    /// `None` to leave `opts` untouched.
    ///
    /// The hook receives a reference to the planned `opts`, not ownership — it
    /// must not retain the borrow (the closure clones before `await`). Runs on
    /// the blocking-bridge thread inside the harness `StreamFn` closure.
    fn before_request(
        &self,
        _model: &Model,
        _ctx: &Context,
        _opts: &SimpleStreamOptions,
    ) -> Option<SimpleStreamOptionsPatch> {
        None
    }

    /// Fired after the stream resolves with the terminal assistant message.
    /// `message` is the final `AssistantMessage` the provider produced (the same
    /// one the loop emits as `MessageEnd`). `None` lets the default no-op impl
    /// apply. Runs on the blocking-bridge thread.
    fn after_response(&self, _model: &Model, _message: &AssistantMessage) {}
}

/// A no-op default so the harness can install a `ProviderHooks` slot cheaply
/// when none is supplied (uniform code path vs `Option<Arc<dyn>>`).
#[derive(Default)]
pub struct NoopProviderHooks;
impl ProviderHooks for NoopProviderHooks {}

/// A patch against the model-facing [`Context`] returned by
/// [`ProviderHooks::transform_context`].
///
/// Mirrors the `systemPrompt` half of pi's `transform_context` result
/// (`harness/agent-harness.ts:445`): the hook may replace the system prompt the
/// request carries. pi also allows replacing `messages` there; rpi deliberately
/// does not (yet) — see `ProviderHooks::transform_context`.
///
/// `None` inside means "leave as-is", so an empty patch is a no-op and a caller
/// that only wants to observe can return `None` from the hook entirely.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextPatch {
    /// Replacement system prompt for this request. `Some` installs it verbatim
    /// (**replacement, not append** — a plugin that wants to append reads
    /// `ctx.system_prompt` and concatenates, exactly as pi requires).
    pub system_prompt: Option<String>,
}

impl ContextPatch {
    /// Apply this patch to `ctx` in place.
    pub fn apply(&self, ctx: &mut Context) {
        if let Some(prompt) = &self.system_prompt {
            ctx.system_prompt = Some(prompt.clone());
        }
    }
}

/// A patch against [`SimpleStreamOptions`] returned by [`ProviderHooks::before_request`].
/// Mirrors TS `beforeRequest`'s per-request overrides but targets the live
/// `SimpleStreamOptions` (the type `stream_simple` receives), not the harness
/// pinned subset. `Option<Option<T>>` semantics: outer `None` = "leave this
/// field"; inner `Some(None)` on the headers/metadata maps = "delete this key".
///
/// This is *distinct* from the harness-side `AgentHarnessStreamOptionsPatch`
/// (which lives in rpi-harness and patches `AgentHarnessStreamOptions`). Both
/// exist; this one is the rpi-ai seam so provider hooks can be defined without
/// a rpi-harness cycle.
#[derive(Debug, Clone, Default)]
pub struct SimpleStreamOptionsPatch {
    pub timeout: Option<Option<Duration>>,
    /// Replace the idle timeout, reset its default with inner `None`, or disable with zero.
    pub stream_idle_timeout: Option<Option<Duration>>,
    pub max_retries: Option<Option<u32>>,
    pub max_retry_delay: Option<Option<Duration>>,
    /// `None` = leave; `Some(map)` = merge, inner `None` deletes a key.
    pub headers: Option<BTreeMap<String, Option<String>>>,
    /// `None` = leave; `Some(map)` = merge, inner `None` deletes a key.
    pub metadata: Option<BTreeMap<String, Option<String>>>,
    pub cache_retention: Option<Option<CacheRetention>>,
    pub max_tokens: Option<Option<u64>>,
    pub temperature: Option<Option<f64>>,
    pub session_id: Option<Option<String>>,
}

impl SimpleStreamOptionsPatch {
    /// Apply this patch to `opts` in place. Mirrors the harness patch semantics:
    /// outer `None` = leave the field; an explicit value replaces it; `None`
    /// inside a map entry deletes that key.
    pub fn apply(&self, opts: &mut SimpleStreamOptions) {
        if let Some(v) = self.timeout {
            opts.timeout = v;
        }
        if let Some(v) = self.stream_idle_timeout {
            opts.stream_idle_timeout = v;
        }
        if let Some(v) = self.max_retries {
            opts.max_retries = v;
        }
        if let Some(v) = self.max_retry_delay {
            opts.max_retry_delay = v;
        }
        if let Some(patch) = &self.headers {
            let mut map = opts.headers.take().unwrap_or_default();
            for (k, v) in patch {
                set_http_header(&mut map, k, v.as_deref());
            }
            opts.headers = if map.is_empty() { None } else { Some(map) };
        }
        if let Some(patch) = &self.metadata {
            let mut map = opts.metadata.take().unwrap_or_default();
            for (k, v) in patch {
                match v {
                    Some(val) => map.insert(k.clone(), val.clone()),
                    None => map.remove(k),
                };
            }
            opts.metadata = if map.is_empty() { None } else { Some(map) };
        }
        if let Some(v) = self.cache_retention {
            opts.cache_retention = v.unwrap_or_default();
        }
        if let Some(v) = self.max_tokens {
            opts.max_tokens = v;
        }
        if let Some(v) = self.temperature {
            opts.temperature = v;
        }
        if let Some(v) = &self.session_id {
            opts.session_id = v.clone();
        }
    }
}
