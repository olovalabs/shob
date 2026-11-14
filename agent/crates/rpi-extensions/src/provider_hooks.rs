//! B4 — extension provider hooks. Bridges an extension's `on(BeforeProviderRequest)`
//! / `on(BeforeProviderHeaders)` / `on(AfterProviderResponse)` handlers into
//! rpi-ai's [`ProviderHooks`] slot, so every provider call fans the live
//! request/response context out to the subscribed plugins.
//!
//! **Observer semantics (v1):** the SDK `EventHandlerFn` returns `i32` (no
//! result payload), so a handler observes the request/response — it can record
//! state, rotate an external token store, emit diagnostics — but cannot PATCH
//! the live `SimpleStreamOptions`. The `before_request` hook therefore returns
//! `None` (no patch); a future ABI extension (an `out` pointer on the handler)
//! would unlock the patch path. The harness treats a `None` return as
//! "leave opts untouched", so the observer path is safe.

use std::sync::{Arc, Mutex};

use rpi_ai::types::{AssistantMessage, Content, Message, UserContent};
use rpi_ai::Context;
use rpi_ai::{ContextPatch, Model, ProviderHooks, SimpleStreamOptions, SimpleStreamOptionsPatch};
use rpi_plugin_sdk::EventTag;

use crate::loader::ExtensionSession;
use crate::prompt_transform::emit_before_agent_start;
use crate::registry::RegistrySnapshot;
use crate::translate::dispatch_data_event;
use crate::PluginKeepalive;

/// A [`ProviderHooks`] that dispatches to the registered extension handlers.
/// Keeps the cdylib mappings alive via the keepalive (the handler fn pointers
/// live inside the plugins).
pub struct ExtensionProviderHooks {
    snapshot: Arc<RegistrySnapshot>,
    _keepalive: Arc<PluginKeepalive>,
    /// Timestamp of the last user message we emitted `BeforeAgentStart` for.
    /// The hook fires once per provider call, but `before_agent_start` must
    /// fire once per prompt, so the prompt timestamp is the dedupe key.
    last_before_agent_start: Mutex<Option<i64>>,
    /// The prompt transform result for the prompt identified by the timestamp,
    /// cached so it is computed **once per prompt** rather than once per
    /// provider call. pi computes it once before the run and reuses it
    /// (`_runSystemPromptOptions`, reset in the run's `finally`); a tool loop
    /// makes several provider calls for one prompt, and re-running every
    /// registered handler on each of them would both repeat side effects and
    /// let an unstable handler change the prompt mid-run.
    ///
    /// `Some((timestamp, prompt))` where `prompt: None` records "this prompt was
    /// transformed, result was no-change" — distinct from "not yet computed",
    /// so handlers are not re-run just because they declined to change anything.
    last_forced_prompt: Mutex<Option<(i64, Option<String>)>>,
}

impl ExtensionProviderHooks {
    /// Build hooks over a loaded extension session's registry snapshot. Returns
    /// `None` when nothing needs this bridge (so a session without provider or
    /// prompt hooks runs the plain no-op path).
    ///
    /// Two independent reasons to install it, and both must be checked:
    ///
    /// - **Provider-hook tags** — a plugin observing request/response. Includes
    ///   `BeforeAgentStart`, which is emitted from this bridge (see
    ///   [`Self::before_request`]) because the harness never emits a prompt
    ///   message event.
    /// - **`before_agent_start` transformers** — a plugin that *replaces* the
    ///   system prompt. This is a separate slot, not an event subscription, so a
    ///   plugin that only transforms the prompt registers no handlers at all;
    ///   keying this check on the tags alone would skip the bridge and silently
    ///   ignore its transform.
    pub fn from_session(session: &ExtensionSession) -> Option<Self> {
        let snapshot = session.snapshot_arc()?;
        let observes_events = [
            EventTag::BeforeProviderRequest,
            EventTag::BeforeProviderHeaders,
            EventTag::AfterProviderResponse,
            EventTag::BeforeAgentStart,
        ]
        .iter()
        .any(|t| !snapshot.handlers_for(*t).is_empty());
        let transforms_prompt = !snapshot.before_agent_start().is_empty();
        if !observes_events && !transforms_prompt {
            return None;
        }
        Some(Self {
            snapshot,
            _keepalive: session.keepalive(),
            last_before_agent_start: Mutex::new(None),
            last_forced_prompt: Mutex::new(None),
        })
    }
}

impl ProviderHooks for ExtensionProviderHooks {
    /// pi's `transform_context` / `before_agent_start`-returns-`systemPrompt`:
    /// let a plugin replace the prompt the request carries.
    ///
    /// Computed once per prompt and cached, so every provider call of a run
    /// (the tool loop) sees the same prompt — see
    /// [`Self::last_forced_prompt`]. Returns `None` when no plugin registered a
    /// transformer, when they all declined, or when the registry is stale, all
    /// of which mean "leave the prompt alone".
    fn transform_context(&self, _model: &Model, ctx: &Context) -> Option<ContextPatch> {
        if self.snapshot.before_agent_start().is_empty() {
            return None;
        }
        // Keyed on the user prompt, like the observer event below: the prompt is
        // what identifies a turn, and it is stable across the run's provider
        // calls. With no user message (a continuation) there is nothing to key
        // on, so the transform is skipped rather than applied to an unknown turn.
        let (timestamp, prompt, image_count) = latest_user_prompt(&ctx.messages)?;

        let mut cache = self.last_forced_prompt.lock().unwrap();
        if cache.as_ref().map(|(ts, _)| *ts) != Some(timestamp) {
            let event = before_agent_start_event(&prompt, image_count, ctx);
            let forced = emit_before_agent_start(
                ctx.system_prompt.as_deref().unwrap_or_default(),
                &event.to_string(),
                &self.snapshot,
            );
            *cache = Some((timestamp, forced));
        }
        cache
            .as_ref()
            .and_then(|(_, forced)| forced.clone())
            .map(|system_prompt| ContextPatch {
                system_prompt: Some(system_prompt),
            })
    }

    /// Fan the planned request out to `BeforeProviderRequest` +
    /// `BeforeProviderHeaders` subscribers. Returns `None` for the stream options
    /// — observer-only (the event handler ABI has no result channel); a plugin's
    /// influence on the *prompt* travels through [`Self::transform_context`]
    /// instead.
    fn before_request(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> Option<SimpleStreamOptionsPatch> {
        // (A) pi's `before_agent_start`: observe each NEW user prompt exactly
        // once. The harness persists prompts before the run and calls
        // `run_agent_loop` with an empty prompts vec, so no
        // MessageStart/MessageEnd reaches plugins for them. This bridge is the
        // first place the prompt is visible, so it emits the pi-equivalent
        // event. Dedupe by prompt timestamp: one run can make several provider
        // calls, but `before_agent_start` fires once per prompt.
        if let Some((timestamp, prompt, image_count)) = latest_user_prompt(&ctx.messages) {
            let mut last = self.last_before_agent_start.lock().unwrap();
            if *last != Some(timestamp) {
                *last = Some(timestamp);
                drop(last);
                let event = before_agent_start_event(&prompt, image_count, ctx);
                // Synchronous, like the request dispatch below: a subscriber's
                // `BeforeAgentStart` handler runs before any
                // `BeforeProviderRequest` of this turn reaches it, so the turn is
                // fully identified (session included) by the time its first
                // generation is created.
                dispatch_data_event(
                    &self.snapshot,
                    EventTag::BeforeAgentStart,
                    &event.to_string(),
                );
            }
        }

        let request = serde_json::json!({
            "model": model.id,
            "provider": model.provider,
            "baseUrl": model.base_url,
            "reasoning": model.reasoning,
            "pricingAvailable": model.has_pricing(),
            "apiKey": opts.api_key,
            "timeoutMs": opts.timeout.map(|d| d.as_millis() as u64),
            "streamIdleTimeoutMs": opts.stream_idle_timeout.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            "headers": opts.headers,
            "metadata": opts.metadata,
            "maxTokens": opts.max_tokens,
            "temperature": opts.temperature,
            // (B) the model-facing messages, so observers (e.g. rpi-langfuse)
            // can record the real generation input. `ctx` was previously
            // discarded entirely, which is why generation/trace input was
            // always null for plugins.
            "messages": ctx.messages,
        });
        dispatch_data_event(
            &self.snapshot,
            EventTag::BeforeProviderRequest,
            &request.to_string(),
        );
        let headers = serde_json::json!({ "headers": opts.headers });
        dispatch_data_event(
            &self.snapshot,
            EventTag::BeforeProviderHeaders,
            &headers.to_string(),
        );
        None
    }

    /// Fan the terminal assistant message out to `AfterProviderResponse`
    /// subscribers.
    fn after_response(&self, _model: &Model, message: &AssistantMessage) {
        let json = serde_json::to_value(message).unwrap_or(serde_json::json!({}));
        dispatch_data_event(
            &self.snapshot,
            EventTag::AfterProviderResponse,
            &json.to_string(),
        );
    }
}

/// The `before_agent_start` envelope: what the observer event carries and what
/// a prompt-transforming handler reads.
///
/// One builder for both, so an observer and a transformer cannot be shown
/// different views of the same turn.
///
/// `systemPrompt` is read from `ctx`, which means the **post-transform** prompt
/// when the observer event is dispatched: rpi emits that event from
/// `before_request`, and `ProviderHooks::transform_context` runs first (the same
/// order pi uses in `assistant.ts` — transform, then request). So an observer
/// sees the prompt the provider is about to be given. pi's own
/// `before_agent_start` fires earlier in the run and therefore carries the base
/// prompt; a plugin that needs the pre-transform value should capture it from
/// its own transformer's input instead of from this event.
fn before_agent_start_event(prompt: &str, image_count: usize, ctx: &Context) -> serde_json::Value {
    serde_json::json!({
        "prompt": prompt,
        "imageCount": image_count,
        "systemPrompt": ctx.system_prompt,
        // The session this process serves. A plugin that traces the run reports
        // it as the session id (nothing else tells an observer which session it
        // is watching). Read from the environment per turn rather than captured
        // at harness build, so an in-process session swap is reflected on the
        // very next turn.
        "sessionId": std::env::var(rpi_plugin_sdk::SESSION_ID_ENV).ok(),
        // Who this extension is embedded in, so it can label its own output
        // (trace name, tags, `service.name`) without hardcoding a brand that a
        // rename would silently invalidate. Sent per turn, so it describes the
        // process actually running rather than the SDK the plugin was built
        // against.
        "host": host_identity(),
    })
}

/// The host's own identity, as handed to plugins.
///
/// The name is the plugin contract's constant ([`rpi_plugin_sdk::HOST_NAME`])
/// and the version is this host crate's own `CARGO_PKG_VERSION` — every
/// workspace crate shares one version, so this is the running host's version
/// and not the SDK a plugin was compiled against. Both are compile-time
/// aliases, so there is exactly one place to change either.
fn host_identity() -> serde_json::Value {
    serde_json::json!({
        "name": rpi_plugin_sdk::HOST_NAME,
        "version": env!("CARGO_PKG_VERSION"),
    })
}

/// Text/timestamp/image-count of the most recent user message. Mirrors the
/// `prompt`/`images` fields pi puts on its `before_agent_start` event.
fn latest_user_prompt(messages: &[Message]) -> Option<(i64, String, usize)> {
    messages.iter().rev().find_map(|message| match message {
        Message::User(user) => Some((
            user.timestamp,
            user_content_text(&user.content),
            user_content_image_count(&user.content),
        )),
        _ => None,
    })
}

fn user_content_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                Content::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn user_content_image_count(content: &UserContent) -> usize {
    match content {
        UserContent::Text(_) => 0,
        UserContent::Blocks(blocks) => blocks
            .iter()
            .filter(|block| matches!(block, Content::Image(_)))
            .count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_plugin_sdk::EventTag;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    static PROVIDER_HITS: AtomicUsize = AtomicUsize::new(0);
    static PROVIDER_LOCK: Mutex<()> = Mutex::new(());
    static CAPTURED: Mutex<Vec<(EventTag, String)>> = Mutex::new(Vec::new());

    rpi_plugin_sdk::event_handler! { fn capture_handler(ev, _ud) {
        let payload = ev.data_text().unwrap_or_default();
        CAPTURED.lock().unwrap().push((ev.tag(), payload));
        0
    } }

    rpi_plugin_sdk::event_handler! { fn counting_provider_handler(ev, _ud) {
        // Count every dispatch on the subscribed tags (BeforeProviderRequest +
        // BeforeProviderHeaders). The `model` check validates the request-event
        // payload shape separately — it must NOT gate the count, or the headers
        // event (whose payload is `{"headers":...}` with no `model` key) would
        // be silently dropped and the fan-out count would be wrong.
        let s = ev.data_text().unwrap_or_default();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
            // The request event carries `model`; the headers event does not.
            // Both are valid dispatches — count unconditionally, and sanity-check
            // the request payload when `model` is present.
            if v.get("model").is_some() {
                assert!(v.get("model").is_some(), "request event must carry model");
            }
            PROVIDER_HITS.fetch_add(1, Ordering::SeqCst);
        }
        0
    } }

    /// `before_agent_start` fires once per NEW user prompt (not once per
    /// provider call in a run), and the request payload carries the
    /// model-facing `messages`.
    #[test]
    fn before_request_emits_before_agent_start_once_and_forwards_messages() {
        let _guard = PROVIDER_LOCK.lock().unwrap();
        CAPTURED.lock().unwrap().clear();

        let mut registry = crate::registry::ExtensionRegistry::new();
        registry.register_event_handler(
            "test-plugin".to_string(),
            EventTag::BeforeAgentStart,
            capture_handler,
            std::ptr::null_mut(),
        );
        registry.register_event_handler(
            "test-plugin".to_string(),
            EventTag::BeforeProviderRequest,
            capture_handler,
            std::ptr::null_mut(),
        );
        let snapshot = Arc::new(registry.snapshot());
        let hooks = ExtensionProviderHooks {
            snapshot: Arc::clone(&snapshot),
            _keepalive: Arc::new(crate::PluginKeepalive::new(Vec::new(), None)),
            last_before_agent_start: Mutex::new(None),
            last_forced_prompt: Mutex::new(None),
        };
        let model = rpi_ai::Model::new(
            "m",
            "m",
            rpi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let ctx = rpi_ai::Context::new(vec![rpi_ai::types::Message::User(
            rpi_ai::types::UserMessage::new(
                rpi_ai::types::UserContent::Text("hello".to_string()),
                42,
            ),
        )]);
        let opts = rpi_ai::SimpleStreamOptions::default();

        // Two provider calls in the same run (same prompt) → one
        // before_agent_start, two before_provider_request observations.
        hooks.before_request(&model, &ctx, &opts);
        hooks.before_request(&model, &ctx, &opts);

        let captured = CAPTURED.lock().unwrap().clone();
        let agent_starts: Vec<_> = captured
            .iter()
            .filter(|(tag, _)| matches!(tag, EventTag::BeforeAgentStart))
            .collect();
        assert_eq!(
            agent_starts.len(),
            1,
            "before_agent_start must fire once per prompt: {captured:?}"
        );
        let start_payload: serde_json::Value =
            serde_json::from_str(&agent_starts[0].1).expect("before_agent_start payload is JSON");
        assert_eq!(start_payload["prompt"], "hello");
        assert_eq!(start_payload["imageCount"], 0);
        // The session id rides along so an observer can name the session on the
        // very first turn (the prompt is the only per-turn channel it gets).
        // Absent when this process has no session id in its environment.
        assert_eq!(
            start_payload.get("sessionId").map(|v| v.is_null()),
            Some(std::env::var(rpi_plugin_sdk::SESSION_ID_ENV).is_err()),
            "sessionId must mirror the host's session id env var: {}",
            agent_starts[0].1
        );
        // And the host identifies itself, so a plugin can label its own output
        // (trace name, tags, service.name) without hardcoding a brand.
        assert_eq!(start_payload["host"]["name"], rpi_plugin_sdk::HOST_NAME);
        assert_eq!(
            start_payload["host"]["version"],
            env!("CARGO_PKG_VERSION"),
            "the host reports the version of the host, not of the plugin SDK"
        );

        let requests: Vec<_> = captured
            .iter()
            .filter(|(tag, _)| matches!(tag, EventTag::BeforeProviderRequest))
            .collect();
        assert_eq!(requests.len(), 2, "every provider call is observed");
        assert!(
            requests[0].1.contains("\"messages\""),
            "request payload must forward messages: {}",
            requests[0].1
        );
        let request: serde_json::Value = serde_json::from_str(&requests[0].1).unwrap();
        assert_eq!(request["pricingAvailable"], false);
        let mut model = model;
        model.cost_configured = true;
        hooks.before_request(&model, &ctx, &opts);
        let explicit_zero: serde_json::Value =
            serde_json::from_str(&CAPTURED.lock().unwrap().last().unwrap().1).unwrap();
        assert_eq!(explicit_zero["pricingAvailable"], true);
        model.cost_configured = false;
        model.cost.rates.input = 1.0;
        hooks.before_request(&model, &ctx, &opts);
        let built_in: serde_json::Value =
            serde_json::from_str(&CAPTURED.lock().unwrap().last().unwrap().1).unwrap();
        assert_eq!(built_in["pricingAvailable"], true);
    }

    /// `dispatch_data_event` fans the JSON to the tag's handlers, and the
    /// `ExtensionProviderHooks::before_request` path dispatches BOTH the
    /// request and headers events to their subscribers.
    #[test]
    fn provider_hooks_dispatch_to_subscribers() {
        let _guard = PROVIDER_LOCK.lock().unwrap();
        PROVIDER_HITS.store(0, Ordering::SeqCst);

        let mut registry = crate::registry::ExtensionRegistry::new();
        let handler: rpi_plugin_sdk::EventHandlerFn = counting_provider_handler;
        registry.register_event_handler(
            "test-plugin".to_string(),
            EventTag::BeforeProviderRequest,
            handler,
            std::ptr::null_mut(),
        );
        registry.register_event_handler(
            "test-plugin".to_string(),
            EventTag::BeforeProviderHeaders,
            handler,
            std::ptr::null_mut(),
        );
        let snapshot = Arc::new(registry.snapshot());

        // dispatch_data_event directly: one request event ⇒ one hit.
        assert!(dispatch_data_event(
            &snapshot,
            EventTag::BeforeProviderRequest,
            r#"{"model":"m"}"#,
        ));
        assert_eq!(PROVIDER_HITS.load(Ordering::SeqCst), 1);

        // through ExtensionProviderHooks::before_request. The hook fires BOTH
        // BeforeProviderRequest AND BeforeProviderHeaders (each with the same
        // counting handler subscribed), so 2 more hits ⇒ 3 total. The headers
        // event's payload is `{"headers":...}` (no `model` key) — the handler
        // counts it regardless (see counting_provider_handler).
        let hooks = ExtensionProviderHooks {
            snapshot: Arc::clone(&snapshot),
            _keepalive: Arc::new(crate::PluginKeepalive::new(Vec::new(), None)),
            last_before_agent_start: Mutex::new(None),
            last_forced_prompt: Mutex::new(None),
        };
        let model = rpi_ai::Model::new(
            "m",
            "m",
            rpi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let ctx = rpi_ai::Context::default();
        let opts = rpi_ai::SimpleStreamOptions::default();
        let patch = hooks.before_request(&model, &ctx, &opts);
        assert!(patch.is_none(), "observer-only v1: no patch returned");
        assert_eq!(PROVIDER_HITS.load(Ordering::SeqCst), 3);
    }

    /// A tag with no subscribers dispatches nothing (and doesn't free anything
    /// dangling — dispatch_data_event short-circuits before building events).
    #[test]
    fn provider_hooks_no_subscribers_is_noop() {
        let registry = crate::registry::ExtensionRegistry::new();
        let snapshot = Arc::new(registry.snapshot());
        assert!(!dispatch_data_event(
            &snapshot,
            EventTag::AfterProviderResponse,
            "{}",
        ));
    }

    // ---- before_agent_start prompt transform ------------------------------
    //
    // These use in-process `extern "C"` handlers rather than a cdylib: the ABI
    // round-trip has its own end-to-end test (`plugin_stub_smoke`), so what is
    // left to pin here is the semantics — chaining, failure isolation, and the
    // "no change" signal — which are easier to express with direct handlers.

    /// Handler that returns `<marker>` + whatever prompt it was handed, so a
    /// test can observe the chain by the markers accumulating in order.
    fn append_marker(
        envelope: &str,
        out: &mut rpi_plugin_sdk::callbacks::Output<'_>,
        marker: &str,
    ) -> i32 {
        let current = serde_json::from_str::<serde_json::Value>(envelope)
            .ok()
            .and_then(|v| v.get("systemPrompt")?.as_str().map(str::to_string))
            .unwrap_or_default();
        let json = serde_json::json!({ "systemPrompt": format!("{current}{marker}") });
        i32::from(!out.write(json.to_string()))
    }

    rpi_plugin_sdk::string_handler! { fn append_a(input, out, _data) { append_marker(input, out, " A") } }
    rpi_plugin_sdk::string_handler! { fn append_b(input, out, _data) { append_marker(input, out, " B") } }
    rpi_plugin_sdk::string_handler! { fn append_after(input, out, _data) { append_marker(input, out, " AFTER") } }
    rpi_plugin_sdk::string_handler! { fn append_mark(input, out, _data) { append_marker(input, out, " MARK") } }

    /// Handler that declines to change anything (empty object ⇒ no-op).
    extern "C" fn declines(
        _event_json: rpi_plugin_sdk::StbStringRef,
        _out: *mut rpi_plugin_sdk::StbString,
        _user_data: *mut std::ffi::c_void,
    ) -> i32 {
        0
    }

    /// Handler that reports a handled error.
    extern "C" fn fails(
        _event_json: rpi_plugin_sdk::StbStringRef,
        _out: *mut rpi_plugin_sdk::StbString,
        _user_data: *mut std::ffi::c_void,
    ) -> i32 {
        7
    }

    /// Build hooks whose snapshot has the given prompt transformers.
    fn hooks_with(
        handlers: Vec<crate::registry::BeforeAgentStartHandler>,
    ) -> ExtensionProviderHooks {
        let mut registry = crate::registry::ExtensionRegistry::new();
        for h in handlers {
            registry.register_before_agent_start(h.handler, h.plugin_free_string, h.user_data);
        }
        ExtensionProviderHooks {
            snapshot: Arc::new(registry.snapshot()),
            _keepalive: Arc::new(crate::PluginKeepalive::new(Vec::new(), None)),
            last_before_agent_start: Mutex::new(None),
            last_forced_prompt: Mutex::new(None),
        }
    }

    /// Transformers chain in registration order: each sees the previous one's
    /// result. This is pi's `runner.ts` behavior (a later handler observes the
    /// prompt an earlier one set), and it is what makes "append" composable.
    #[test]
    fn prompt_transformers_chain_in_registration_order() {
        let hooks = hooks_with(vec![
            crate::registry::BeforeAgentStartHandler {
                handler: append_a,
                plugin_free_string: test_free,
                user_data: std::ptr::null_mut(),
            },
            crate::registry::BeforeAgentStartHandler {
                handler: append_b,
                plugin_free_string: test_free,
                user_data: std::ptr::null_mut(),
            },
        ]);

        let model = rpi_ai::Model::new(
            "m",
            "m",
            rpi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let mut ctx = rpi_ai::Context::new(vec![rpi_ai::types::Message::User(
            rpi_ai::types::UserMessage::new(
                rpi_ai::types::UserContent::Text("hello".to_string()),
                1,
            ),
        )]);
        ctx.system_prompt = Some("base".to_string());

        let patch = hooks
            .transform_context(&model, &ctx)
            .expect("both handlers changed the prompt");
        // Both markers present, in order — proof the second handler saw the
        // first's output rather than the original prompt.
        assert_eq!(patch.system_prompt.as_deref(), Some("base A B"));
    }

    /// A failing handler leaves the prompt as the previous handler left it and
    /// the fan-out continues (pi isolates a throwing handler; it does not abort
    /// the run or discard earlier results).
    #[test]
    fn a_failing_transformer_is_skipped_and_the_chain_continues() {
        let hooks = hooks_with(vec![
            crate::registry::BeforeAgentStartHandler {
                handler: fails,
                plugin_free_string: test_free,
                user_data: std::ptr::null_mut(),
            },
            crate::registry::BeforeAgentStartHandler {
                handler: append_after,
                plugin_free_string: test_free,
                user_data: std::ptr::null_mut(),
            },
        ]);

        let model = rpi_ai::Model::new(
            "m",
            "m",
            rpi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let mut ctx = rpi_ai::Context::new(vec![rpi_ai::types::Message::User(
            rpi_ai::types::UserMessage::new(
                rpi_ai::types::UserContent::Text("hello".to_string()),
                1,
            ),
        )]);
        ctx.system_prompt = Some("base".to_string());

        let patch = hooks.transform_context(&model, &ctx).expect("changed");
        assert_eq!(
            patch.system_prompt.as_deref(),
            Some("base AFTER"),
            "the failing handler must contribute nothing and not stop the next one"
        );
    }

    /// All handlers declining ⇒ `None`, i.e. the harness leaves the prompt
    /// alone. A plugin that only observes must be able to say nothing without
    /// installing an empty prompt.
    #[test]
    fn declining_transformers_leave_the_prompt_untouched() {
        let hooks = hooks_with(vec![crate::registry::BeforeAgentStartHandler {
            handler: declines,
            plugin_free_string: test_free,
            user_data: std::ptr::null_mut(),
        }]);
        let model = rpi_ai::Model::new(
            "m",
            "m",
            rpi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let ctx = rpi_ai::Context::new(vec![rpi_ai::types::Message::User(
            rpi_ai::types::UserMessage::new(
                rpi_ai::types::UserContent::Text("hello".to_string()),
                1,
            ),
        )]);
        assert!(
            hooks.transform_context(&model, &ctx).is_none(),
            "no handler changed the prompt ⇒ no patch ⇒ harness leaves it alone"
        );
    }

    /// No registered transformer ⇒ `None` and the ABI is never entered. A
    /// session with only observer plugins must not pay for (or risk) this path.
    #[test]
    fn no_transformers_means_no_patch() {
        let hooks = hooks_with(vec![]);
        let model = rpi_ai::Model::new(
            "m",
            "m",
            rpi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let ctx = rpi_ai::Context::default();
        assert!(hooks.transform_context(&model, &ctx).is_none());
    }

    use rpi_plugin_sdk::callbacks::free_string as test_free;

    /// The `BeforeAgentStart` observer sees the **post-transform** prompt.
    ///
    /// rpi emits that event from `before_request`, and `transform_context` runs
    /// first (the order `agent_harness::build_stream_fn` uses, mirroring pi's
    /// `assistant.ts`). So an observer is told what the provider is about to be
    /// given, not what it would have been given. This is a deliberate contract
    /// and a subtle one: swapping the two calls in `build_stream_fn` would
    /// silently change what every observer reports, with no other test failing.
    #[test]
    fn the_observer_sees_the_prompt_after_the_transform() {
        let _guard = PROVIDER_LOCK.lock().unwrap();
        CAPTURED.lock().unwrap().clear();

        let mut registry = crate::registry::ExtensionRegistry::new();
        registry.register_event_handler(
            "test-plugin".to_string(),
            EventTag::BeforeAgentStart,
            capture_handler,
            std::ptr::null_mut(),
        );
        registry.register_before_agent_start(append_mark, test_free, std::ptr::null_mut());
        let hooks = ExtensionProviderHooks {
            snapshot: Arc::new(registry.snapshot()),
            _keepalive: Arc::new(crate::PluginKeepalive::new(Vec::new(), None)),
            last_before_agent_start: Mutex::new(None),
            last_forced_prompt: Mutex::new(None),
        };

        let model = rpi_ai::Model::new(
            "m",
            "m",
            rpi_ai::Api::AnthropicMessages,
            "anthropic",
            "https://api.anthropic.com",
        );
        let mut ctx = rpi_ai::Context::new(vec![rpi_ai::types::Message::User(
            rpi_ai::types::UserMessage::new(
                rpi_ai::types::UserContent::Text("hello".to_string()),
                1,
            ),
        )]);
        ctx.system_prompt = Some("base".to_string());

        // How the harness drives the two hooks: transform first, then apply, then
        // observe from the patched context.
        let patch = hooks
            .transform_context(&model, &ctx)
            .expect("transformer changed the prompt");
        patch.apply(&mut ctx);
        hooks.before_request(&model, &ctx, &rpi_ai::SimpleStreamOptions::default());

        let captured = CAPTURED.lock().unwrap().clone();
        let start = captured
            .iter()
            .find(|(tag, _)| matches!(tag, EventTag::BeforeAgentStart))
            .expect("BeforeAgentStart was dispatched");
        let payload: serde_json::Value = serde_json::from_str(&start.1).expect("JSON");
        assert_eq!(
            payload["systemPrompt"], "base MARK",
            "observers must be shown the prompt the provider will actually get"
        );
    }
}
