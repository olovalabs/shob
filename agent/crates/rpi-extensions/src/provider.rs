//! B5c — `PluggableProvider`: the host-side `Provider` impl wrapping a plugin's
//! `ProviderRequestFn`.
//!
//! A plugin's [`ProviderRequestFn`](rpi_plugin_sdk::ProviderRequestFn) is a
//! **synchronous one-shot**: given a borrowed JSON request envelope it writes a
//! **plugin-owned** `out` [`StbString`] (a full assistant-message JSON). It
//! cannot drive a chunked `stream_simple` without blocking, so the v1
//! `PluggableProvider` emits the full response as a **single terminal `Done`
//! chunk** — a documented divergence from pi's async `streamSimple`
//! (`custom-provider.md`). The ffi call runs on `spawn_blocking` (via a captured
//! [`Handle`](tokio::runtime::Handle) — same foreign-thread fix as the
//! `runtime_action` bridge), so the async `stream_simple` never blocks a runtime
//! worker.
//!
//! ## Request/response envelope
//!
//! - **Request** (borrowed `StbStringRef` handed to `request_fn`): JSON
//!   `{ model, context, options, capabilities }`, where `model`/`context` are
//!   the serde serializations of [`Model`](rpi_ai::Model)/[`Context`](rpi_ai::Context),
//!   `options` is a hand-built JSON of the [`SimpleStreamOptions`] fields
//!   (that type is not `Serialize`; we mirror the field set
//!   [`ExtensionProviderHooks`](crate::ExtensionProviderHooks) dispatches),
//!   and `capabilities` is `{ "recordedToolCalls": 1 }`.
//! - **Response** (the plugin-owned `out` `StbString`): an
//!   [`AssistantMessage`] JSON. The host reclaims `out` via the plugin's
//!   `plugin_free_string` (traveled with the registration), parses the JSON, and
//!   pushes it as one `Done` (or `Error` on nonzero rc / parse failure).
//!
//! ## Lifetime + staleness
//!
//! `PluggableProvider` mirrors [`ExtensionProviderHooks`]: it holds an
//! `Arc<RegistrySnapshot>` (the staleness flag is shared — a `/reload`
//! invalidate marks it stale) + an `Arc<PluginKeepalive>` (keeps the cdylib
//! mapped so `request_fn` stays callable) + the [`RegisteredProvider`] record +
//! a `Handle` captured at build time (the host is on the runtime when it
//! constructs providers from the session).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[cfg(test)]
use rpi_ai::event_stream::create_assistant_message_event_stream;
use rpi_ai::event_stream::create_assistant_message_event_stream_with_cancellation;
use rpi_ai::types::{AssistantMessage, Content, DoneReason, ErrorReason, ToolCall, ToolCallType};
use rpi_ai::{
    AssistantMessageEvent, AssistantMessageEventStream, AssistantMessageEventStreamProducer,
    Context, Model, Provider, SimpleStreamOptions,
};
use rpi_plugin_sdk::callbacks::OwnedString;
use rpi_plugin_sdk::{StbString, StbStringRef};
use tokio::runtime::Handle;

use crate::loader::ExtensionSession;
use crate::registry::{RegisteredProvider, RegistrySnapshot};
use crate::PluginKeepalive;

#[cfg(all(test, feature = "provider-performance"))]
mod compact_tests;
#[cfg(test)]
mod lifetime_tests;
#[cfg(all(test, feature = "provider-performance"))]
mod performance;
mod queue;
#[cfg(test)]
mod review183;
#[cfg(test)]
mod stream_tests;

/// Reserve child capacity before spawning the producer; move this ownership
/// into the foreign driver so runtime shutdown cannot unload executing code.
struct ProviderRequestLifetime {
    _keepalive: Arc<PluginKeepalive>,
    _background: Option<rpi_agent::tool_execution_scope::BackgroundToolGuard>,
}
impl ProviderRequestLifetime {
    fn new(keepalive: Arc<PluginKeepalive>) -> Self {
        Self {
            _keepalive: keepalive,
            _background: rpi_agent::tool_execution_scope::track_background_tool(),
        }
    }
}

/// A `Provider` backed by a plugin's sync `ProviderRequestFn`.
///
/// See the module docs for the v1 one-shot streaming model + the request/response
/// envelope. Built from a loaded [`ExtensionSession`]; one `PluggableProvider`
/// per [`RegisteredProvider`] in the session's snapshot.
pub struct PluggableProvider {
    /// The registration record (id/base_url/api_style + the fn pointers). Cloned
    /// from the snapshot; the fn pointers are valid for the keepalive's lifetime.
    record: RegisteredProvider,
    /// Shared staleness flag (a `/reload` invalidate flows through here).
    snapshot: Arc<RegistrySnapshot>,
    /// Keeps the cdylib mapped so `request_fn` stays callable.
    _keepalive: Arc<PluginKeepalive>,
    /// Captured at build time — `spawn_blocking` works from the async context.
    runtime: Handle,
    models: Vec<Model>,
}

impl PluggableProvider {
    fn new(
        record: RegisteredProvider,
        snapshot: Arc<RegistrySnapshot>,
        keepalive: Arc<PluginKeepalive>,
        runtime: Handle,
    ) -> Arc<Self> {
        let models = snapshot
            .provider_manifest(&record.provider_id)
            .map(|manifest| manifest.models.clone())
            .unwrap_or_default();
        Arc::new(Self {
            record,
            snapshot,
            _keepalive: keepalive,
            runtime,
            models,
        })
    }

    /// Build one [`PluggableProvider`] per registered provider in the session's
    /// snapshot, as `Arc<dyn Provider>`. Returns an empty vec when no plugin
    /// registered a provider (so a session without provider plugins injects
    /// nothing). The `Handle` MUST be captured from a thread running the target
    /// runtime (pi-cli builds providers on the async main thread, same as the
    /// `ActionBridge`).
    pub fn from_session(session: &ExtensionSession, runtime: Handle) -> Vec<Arc<dyn Provider>> {
        let Some(snapshot) = session.snapshot_arc() else {
            return Vec::new();
        };
        let keepalive = session.keepalive();
        snapshot
            .providers()
            .iter()
            .cloned()
            .map(|record| {
                Self::new(
                    record,
                    Arc::clone(&snapshot),
                    Arc::clone(&keepalive),
                    runtime.clone(),
                ) as Arc<dyn Provider>
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl Provider for PluggableProvider {
    fn id(&self) -> &str {
        &self.record.provider_id
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
        let (mut prod, stream) =
            create_assistant_message_event_stream_with_cancellation(&opts.signal);
        let snapshot = Arc::clone(&self.snapshot);
        // into_owned_record copies the fn pointers + owned strings; the
        // `user_data` raw pointer is carried (plugin-owned, valid for the
        // keepalive lifetime).
        let record = self.record.clone();
        let runtime = self.runtime.clone();
        let lifetime = ProviderRequestLifetime::new(Arc::clone(&self._keepalive));
        let model = model.clone();
        let ctx = Arc::new(ctx.clone());
        // `opts` is not `Clone`-free of lifetime — capture the fields the
        // request envelope needs as owned values so the spawned task is
        // `'static`. `SimpleStreamOptions` IS `Clone` (verified, provider.rs:27),
        // so a single clone carries every field into the task.
        let mut opts = opts.clone();
        opts.signal = prod.cancellation_token();

        // `spawn` (not `spawn_blocking`) for the producer task — the producer
        // must be async so it can `await` the blocking ffi call. Inside, we
        // `spawn_blocking` the sync `request_fn` (the ffi call is blocking by
        // contract). This keeps the runtime worker free while the plugin runs.
        tokio::spawn(async move {
            if let Some(stream_fn) = record.stream_fn {
                // Streaming provider: the plugin pushes per-chunk envelopes on
                // the callback while its request fn is still running; the
                // producer forwards each one immediately, so the consumer sees
                // deltas as they arrive instead of one terminal chunk.
                let message = drive_streaming_plugin_provider(
                    &snapshot, &record, stream_fn, runtime, lifetime, &model, &ctx, &opts,
                    &mut prod,
                )
                .await;
                push_terminal(&mut prod, message).await;
            } else {
                let message = drive_plugin_provider(
                    &snapshot, &record, runtime, lifetime, &model, &ctx, &opts,
                )
                .await;
                push_terminal(&mut prod, message).await;
            }
            // `prod` drops here, closing the mpsc sender so the consumer's
            // `next()` returns `None` after the terminal event.
        });

        stream
    }
}

/// Run the sync `ProviderRequestFn` on `spawn_blocking`, read its plugin-owned
/// `out` JSON, reclaim it via `plugin_free_string`, parse to an
/// [`AssistantMessage`]. Returns an error terminal message on any failure
/// (staleness, nonzero rc, parse error, runtime shutdown).
async fn drive_plugin_provider(
    snapshot: &Arc<RegistrySnapshot>,
    record: &RegisteredProvider,
    runtime: Handle,
    lifetime: ProviderRequestLifetime,
    model: &Model,
    ctx: &Context,
    opts: &SimpleStreamOptions,
) -> AssistantMessage {
    // Staleness: a `/reload`d session must not drive its old providers.
    if !snapshot.is_active() {
        return AssistantMessage::terminal(
            model.api.clone(),
            record.provider_id.clone(),
            model.id.clone(),
            rpi_ai::types::StopReason::Error,
            "extensions provider registry is stale (session swapped/reloaded)",
            0,
        );
    }

    let request_json = request_envelope_json(model, ctx, opts);

    // The fn pointers + user_data are `Copy`-able (fn ptrs) or plain raw (ud);
    // clone what spawn_blocking needs. `record` is `Clone` (owned strings).
    let record = record.clone();
    let provider_id = record.provider_id.clone();
    let api = model.api.clone();
    let model_id = model.id.clone();

    // `spawn_blocking` the sync ffi call — `request_fn` is blocking by contract.
    // The join handle is awaited so a panic in the plugin (caught by
    // `catch_unwind` below) surfaces as an error, not a silent hang.
    let request_model_id = model_id.clone();
    let join = runtime.spawn_blocking(move || {
        let _lifetime = lifetime;
        run_provider_request(&record, &request_json, &request_model_id)
    });
    let outcome = match join.await {
        Ok(inner) => inner,
        Err(join_err) => {
            // The blocking task panicked before returning (catch_unwind inside
            // caught a plugin panic and aborted, OR tokio cancelled). Either way
            // no response.
            return AssistantMessage::terminal(
                api,
                provider_id,
                model_id,
                rpi_ai::types::StopReason::Error,
                format!("plugin provider task failed: {join_err}"),
                0,
            );
        }
    };

    match outcome {
        ProviderOutcome::Ok(message) => message,
        ProviderOutcome::Err(msg) => AssistantMessage::terminal(
            api,
            provider_id,
            model_id,
            rpi_ai::types::StopReason::Error,
            msg,
            0,
        ),
    }
}

/// The result of one sync `request_fn` call: either a parsed assistant message
/// or an error string. Computed inside `spawn_blocking`.
enum ProviderOutcome {
    Ok(AssistantMessage),
    Err(String),
}

/// The inner sync driver (runs on the blocking pool). Calls `request_fn`,
/// recovers the plugin-owned `out` `StbString`, reclaims it via
/// `plugin_free_string`, parses to an [`AssistantMessage`]. Wrapped in
/// `catch_unwind` so a plugin panic cannot unwind across FFI (⇒ error outcome).
fn run_provider_request(
    record: &RegisteredProvider,
    request_json: &str,
    model_id: &str,
) -> ProviderOutcome {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_provider_request_inner(record, request_json, model_id)
    }));
    match outcome {
        Ok(inner) => inner,
        Err(_) => {
            tracing::error!(
                "plugin provider {} panicked — refusing to unwind across FFI",
                record.provider_id
            );
            ProviderOutcome::Err(format!(
                "plugin provider {} panicked during request",
                record.provider_id
            ))
        }
    }
}

fn run_provider_request_inner(
    record: &RegisteredProvider,
    request_json: &str,
    model_id: &str,
) -> ProviderOutcome {
    // Borrowed request envelope: the plugin must NOT free it (StbStringRef is a
    // borrow — the SDK contract).
    let req_ref = StbStringRef::from_str(request_json);

    // Plugin-owned `out` — `request_fn` writes a StbString into `*out` (plugin
    // allocation, reclaimed via `plugin_free_string`). Start empty.
    let mut out = StbString::empty();
    let rc = (record.request_fn)(req_ref, &mut out as *mut StbString, record.user_data);
    // SAFETY: the callback transfers its live output to its registered producer.
    let out = unsafe {
        rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(record.plugin_free_string))
    };
    if rc != 0 {
        // Nonzero rc: the plugin signaled failure. Reclaim whatever it wrote
        // (may be empty) before returning.
        return ProviderOutcome::Err(format!(
            "plugin provider {} returned error code {rc}",
            record.provider_id
        ));
    }

    // Read the plugin-owned JSON (borrow `out` for the parse) then reclaim it.
    let response_text = out.as_str_lossy();

    // Parse the response as an AssistantMessage. A plugin that returns a partial
    // or malformed message surfaces a parse error (not a crash) — the host
    // emits it as an `Error` terminal so the run terminates cleanly.
    match serde_json::from_str::<AssistantMessage>(&response_text) {
        Ok(message) => {
            // Stamp the provider/model identity the host expects if the plugin
            // omitted them (a plugin may return only `content`/`stopReason`).
            // We do NOT overwrite fields the plugin set — only fill defaults on
            // a message whose provider/model don't match this provider.
            let mut message = message;
            if message.provider.is_empty() {
                message.provider = record.provider_id.clone();
            }
            if message.model.is_empty() {
                message.model = model_id.to_string();
            }
            ProviderOutcome::Ok(message)
        }
        Err(err) => ProviderOutcome::Err(format!(
            "plugin provider {} returned unparseable response: {err}",
            record.provider_id
        )),
    }
}

/// Push the terminal event for `message`. A `StopReason::Error`/`Aborted`
/// message → `Error` (with the matching `ErrorReason`); otherwise → `Done`
/// (mapping the stop reason onto [`DoneReason`]). This is the single chunk the
/// one-shot path emits, and the terminal event for the streaming path.
async fn push_terminal(prod: &mut AssistantMessageEventStreamProducer, message: AssistantMessage) {
    use rpi_ai::types::StopReason;
    match message.stop_reason {
        // The two failure reasons map to distinct `ErrorReason`s: `Aborted` is
        // the caller going away (a cancellation), not a provider fault. The agent
        // loop and the TUI distinguish them, so collapsing both onto `Error`
        // would report a user-initiated cancel as a provider failure.
        StopReason::Error | StopReason::Aborted => {
            let reason = if message.stop_reason == StopReason::Aborted {
                ErrorReason::Aborted
            } else {
                ErrorReason::Error
            };
            prod.push(AssistantMessageEvent::Error {
                reason,
                error: message,
            })
            .await;
        }
        other => {
            let reason = match other {
                StopReason::Stop => DoneReason::Stop,
                StopReason::Length => DoneReason::Length,
                StopReason::ToolUse => DoneReason::ToolUse,
                StopReason::Deferred => DoneReason::Deferred,
                // Pending/unknown settle to Stop (the plugin should have set a
                // terminal reason; if not, the response is still complete).
                _ => DoneReason::Stop,
            };
            prod.push(AssistantMessageEvent::Done { reason, message })
                .await;
        }
    }
}

/// Build the `options` JSON for the request envelope. `SimpleStreamOptions` is
/// not `Serialize`, so we hand-build the field set (mirrors
/// `ExtensionProviderHooks::before_request`). Sensitive fields (`api_key`) are
/// included — the plugin is trusted host-side code (a cdylib the user installed
/// under `--extensions-dir`); this matches pi's `registerProvider` handing the
/// full options to the plugin's `streamSimple`.
fn options_json(opts: &SimpleStreamOptions) -> serde_json::Value {
    serde_json::json!({
        "apiKey": opts.api_key,
        "timeoutMs": opts.timeout.map(|d| d.as_millis() as u64),
        "streamIdleTimeoutMs": opts.stream_idle_timeout.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        "maxRetries": opts.max_retries,
        "maxRetryDelayMs": opts.max_retry_delay.map(|d| d.as_millis() as u64),
        "headers": opts.headers,
        "metadata": opts.metadata,
        "cacheRetention": format!("{:?}", opts.cache_retention),
        "sessionId": opts.session_id,
        "reasoning": opts.reasoning.map(|r| format!("{r:?}")),
        "maxTokens": opts.max_tokens,
        "temperature": opts.temperature,
        "thinkingBudgets": opts.thinking_budgets,
    })
}

// ---------------------------------------------------------------------------
// Streaming providers (B5c-s) — the per-chunk callback bridge
// ---------------------------------------------------------------------------

/// Run a plugin's [`ProviderStreamRequestFn`](rpi_plugin_sdk::ProviderStreamRequestFn)
/// on `spawn_blocking` while forwarding available deltas to `prod` without a
/// batch-fill delay, then return the plugin's terminal message. Adjacent pure
/// text/thinking deltas already in the queue may be combined before mapping.
///
/// The forwarding is the whole point: the request fn is synchronous, so the
/// callback is the only moment the host learns about a delta. It runs on the
/// blocking-pool thread, inside the plugin's call, and queues chunks for the
/// async driver to forward — which is what makes the consumer's `next()`
/// resolve while the model is still producing.
///
/// Ordering guarantee: chunks are pushed from the single blocking thread, in the
/// order the plugin emits them. On completion the caller waits for the plugin's
/// final message; on cancellation it closes the chunk channel immediately. No
/// chunk can therefore land after the terminal event.
///
/// Cancellation: the caller's signal is selected against incoming chunks and
/// the final join. When it fires the driver (a) latches the control state so the
/// plugin can see it, (b) drops the chunk channel so the plugin's next `emit`
/// reports a rejected chunk, and (c) returns immediately with an `Aborted`
/// terminal rather than waiting out a generation nobody will read. The blocking
/// call itself cannot be interrupted mid-FFI; a plugin that honors the latch
/// unwinds promptly, and one that does not still terminates the run.
#[allow(clippy::too_many_arguments)]
async fn drive_streaming_plugin_provider(
    snapshot: &Arc<RegistrySnapshot>,
    record: &RegisteredProvider,
    stream_fn: rpi_plugin_sdk::ProviderStreamRequestFn,
    runtime: Handle,
    lifetime: ProviderRequestLifetime,
    model: &Model,
    ctx: &Context,
    opts: &SimpleStreamOptions,
    prod: &mut AssistantMessageEventStreamProducer,
) -> AssistantMessage {
    // Staleness: a `/reload`d session must not drive its old providers.
    if !snapshot.is_active() {
        return AssistantMessage::terminal(
            model.api.clone(),
            record.provider_id.clone(),
            model.id.clone(),
            rpi_ai::types::StopReason::Error,
            "extensions provider registry is stale (session swapped/reloaded)",
            0,
        );
    }

    // `options.signal` is the same cancellation contract the built-in providers
    // honor: the agent loop cancels this token when the user aborts the run.
    let cancel = opts.signal.clone();
    let request_json = request_envelope_json(model, ctx, opts);

    // The callback bridge. It is moved (not shared) into the blocking task, and
    // it owns the chunk channel's **only** sender: when that task drops it, the
    // consumer's `recv()` sees the channel close. That lifetime coupling is the
    // reason this is a plain field and not a second `Arc` handle.
    let control = Arc::new(ProviderControlState::default());
    let (tx, mut rx) = queue::channel(64, 4 * 1024 * 1024, Arc::clone(&control));
    let bridge = StreamEmitBridge {
        tx,
        plugin_free_string: record.plugin_free_string,
        control: Some(Arc::clone(&control)),
    };
    let owner = Arc::new(ProviderControlOwner {
        state: Arc::clone(&control),
    });

    let record_owned = record.clone();
    let snapshot_owned = Arc::clone(snapshot);
    let owner_owned = Arc::clone(&owner);
    let api = model.api.clone();
    let provider_id = record.provider_id.clone();
    let model_id = model.id.clone();

    // The callback blocks on bridge capacity while this task forwards events.
    let request_model_id = model_id.clone();
    let join = runtime.spawn_blocking(move || {
        // Cancellation detaches this task; keep the library and host-side
        // context alive until the FFI call and all plugin-owned frees finish.
        let _lifetime = lifetime;
        run_streaming_provider_request(
            &snapshot_owned,
            &record_owned,
            stream_fn,
            &bridge,
            &owner_owned,
            &request_json,
            &request_model_id,
        )
    });

    // One async task maps and forwards chunks; pressure propagates to emit.
    // The queue combines already-buffered pure text/thinking deltas before the
    // mapper creates a full immutable snapshot. It never waits to fill a batch.
    let mut mapper = StreamMapper::new(
        model.api.clone(),
        record.provider_id.clone(),
        model.id.clone(),
    );

    let mut cancelled_mid_stream = false;
    // Forward every chunk, in arrival order, until the channel closes (the
    // blocking task dropping its sender). A chunk the mapper parks behind the
    // host-emitted `Start` is drained on the spot, so ordering holds without a
    // second queue. The cancel arm can fire at any await point — including on the
    // very first `recv`, which is why it is selected rather than checked in the
    // loop body.
    'forward: loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // Latch first: the plugin may be mid-generation and its next
                // poll of `is_cancelled` must see this, not a later race.
                control.cancel();
                cancelled_mid_stream = true;
                break;
            }
            chunk = rx.recv() => match chunk {
                Some(chunk) => {
                    let mut next = Some(chunk);
                    while let Some(chunk) = next {
                        let metadata_only = chunk.get("type").and_then(serde_json::Value::as_str) == Some("usage");
                        if let Some(event) = mapper.apply(chunk) {
                            let accepted = tokio::select! {
                                biased;
                                _ = cancel.cancelled() => false,
                                accepted = prod.push(event) => accepted,
                            };
                            if !accepted {
                                control.cancel();
                                cancelled_mid_stream = true;
                                mapper.output = prod.committed_message().cloned().unwrap_or_else(||
                                    AssistantMessage::empty(model.api.clone(), record.provider_id.clone(), model.id.clone(), 0));
                                break 'forward;
                            }
                        } else if metadata_only {
                            prod.commit_metadata(&mapper.output);
                        }
                        next = mapper.take_deferred();
                    }
                }
                // The plugin finished (or its task ended). Stop forwarding: the
                // terminal event is pushed below, after the join resolves.
                None => break,
            },
        }
    }

    // Normal completion has already drained the channel. Cancellation discards
    // queued chunks so no more content is forwarded after the cancel signal.
    rx.close();
    let mut message = if cancelled_mid_stream {
        // Dropping the join handle detaches the blocking task; waiting here
        // would let an uncooperative plugin hold the entire agent run open.
        mapper.aborted_message()
    } else {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                control.cancel();
                mapper.aborted_message()
            }
            outcome = join => match outcome {
                Ok(message) => message,
                Err(join_err) => AssistantMessage::terminal(
                    api,
                    provider_id,
                    model_id,
                    rpi_ai::types::StopReason::Error,
                    format!("plugin provider task failed: {join_err}"),
                    0,
                ),
            },
        }
    };
    // The agent executes calls from this final message, not from the streamed
    // `ToolCallEnd`s, so a recording seen on the stream must survive into it. A
    // final message that cannot keep it fails the turn instead of letting the
    // call run. An error or aborted message already executes nothing.
    if let Err(reason) = mapper.reconcile_recorded(&mut message) {
        if !matches!(
            message.stop_reason,
            rpi_ai::types::StopReason::Error | rpi_ai::types::StopReason::Aborted
        ) {
            // Constant text: the harness retries errors whose text looks
            // transient, and plugin-chosen ids or names could match that.
            let text =
                format!("plugin provider final message drops a recorded tool call: {reason}");
            message = AssistantMessage {
                usage: message.usage,
                ..error_message(record, &text)
            };
        }
    }
    if message.provider.is_empty() {
        message.provider = record.provider_id.clone();
    }
    if message.model.is_empty() {
        message.model = model.id.clone();
    }
    for event in mapper.chunks_then_message(&message) {
        prod.push(event).await;
    }
    message
}

/// The terminal result of driving a streaming provider: the plugin's own final
/// message, which becomes the `Done`/`Error` payload.
type StreamingProviderOutcome = AssistantMessage;

/// Cancellation latched from `options.signal` for the plugin's synchronous
/// polling and blocking waits. The condition variable wakes every waiter when
/// the async driver observes cancellation.
#[derive(Default)]
struct ProviderControlState {
    cancelled: AtomicBool,
    wait_lock: Mutex<()>,
    wake: Condvar,
    queue: Mutex<Option<std::sync::Weak<queue::Queue>>>,
}

impl ProviderControlState {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Latch cancellation. Idempotent and thread-safe; the plugin may observe it
    /// at any time after this returns.
    fn cancel(&self) {
        // Serialize with the waiter's predicate check to avoid a lost wakeup.
        let _guard = self.wait_lock.lock().unwrap_or_else(|err| err.into_inner());
        self.cancelled.store(true, Ordering::SeqCst);
        self.wake.notify_all();
        if let Some(queue) = self
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
        {
            queue.cancel();
        }
    }

    /// Block up to `timeout_ms` for cancellation. `0` is a non-blocking poll.
    /// Runs on the plugin's request thread and only touches this struct, so it
    /// cannot deadlock on any host lock.
    fn wait(&self, timeout_ms: u32) -> bool {
        if self.is_cancelled() {
            return true;
        }
        if timeout_ms == 0 {
            return false;
        }
        let guard = self.wait_lock.lock().unwrap_or_else(|err| err.into_inner());
        let _wait = self
            .wake
            .wait_timeout_while(guard, Duration::from_millis(u64::from(timeout_ms)), |_| {
                !self.is_cancelled()
            })
            .unwrap_or_else(|err| err.into_inner());
        self.is_cancelled()
    }
}

/// The owner of the control state, kept alive for the whole request. Handing the
/// plugin a pointer to an `Arc`-backed allocation (rather than a stack address)
/// means the address stays valid even if the request task is moved by the
/// blocking pool before it calls the plugin.
struct ProviderControlOwner {
    state: Arc<ProviderControlState>,
}

/// The host's two plugin-facing control trampolines.
///
/// Both take the `user_data` the plugin got from
/// [`rpi_plugin_sdk::ProviderStreamControl`], which is **always** the live
/// `ProviderControlOwner` on the v2 path — so an early return for `user_data ==
/// null` is unreachable from a well-formed plugin and exists only as the
/// defensive floor against a forged pointer.
///
/// `is_cancelled`:
///
/// `chunk_kind` distinguishes content from structural chunks: a cancelled
/// request may still deliver structural frames that close the message the user
/// already saw, but has nobody left to receive model output.
extern "C" fn control_is_cancelled(user_data: *mut c_void, chunk_kind: u32) -> i32 {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if user_data.is_null() {
            // Should not happen: the host always passes a live owner. Report
            // "not cancelled" rather than aborting the plugin's request.
            tracing::error!("provider control callback missing its owner");
            return 0;
        }
        // SAFETY: the owner is heap-allocated and kept alive by the request's
        // `Arc<ProviderControlOwner>` for the whole plugin call.
        let owner = unsafe { &*(user_data as *const ProviderControlOwner) };
        if chunk_kind == rpi_plugin_sdk::ProviderChunkKind::Structural as u32 {
            return 0;
        }
        i32::from(owner.state.is_cancelled())
    }));
    outcome.unwrap_or(0)
}

/// `wait_cancelled_ms`: blocks up to `timeout_ms` on the plugin's own thread and
/// returns nonzero as soon as cancellation is observed.
extern "C" fn control_wait_cancelled_ms(user_data: *mut c_void, timeout_ms: u32) -> i32 {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if user_data.is_null() {
            tracing::error!("provider control callback missing its owner");
            return 0;
        }
        // SAFETY: as in `control_is_cancelled` — the owner outlives the call.
        let owner = unsafe { &*(user_data as *const ProviderControlOwner) };
        i32::from(owner.state.wait(timeout_ms))
    }));
    outcome.unwrap_or(0)
}

/// The callback state an emitted chunk is funneled through. Owned by the host,
/// pointed at by the plugin's `emit_user_data`, and — because it holds the only
/// sender of the chunk channel — the channel's lifetime marker: when the
/// blocking task drops it, the consumer's `recv()` returns `None`.
struct StreamEmitBridge {
    tx: queue::Sender,
    /// The plugin's own `plugin_free_string`: every emitted chunk arrives as a
    /// **plugin-owned** allocation, so only the plugin's deallocator may reclaim
    /// it (the host's would corrupt the plugin's heap).
    plugin_free_string: rpi_plugin_sdk::FreeStringFn,
    /// Latched cancellation shared with the plugin's control callbacks.
    control: Option<Arc<ProviderControlState>>,
}

/// One emitted chunk, already copied out of the plugin's allocation and safely
/// reclaimed through the plugin's `free_string`.
type StreamChunk = serde_json::Value;

/// The host's per-chunk callback. Invoked **synchronously inside the plugin's
/// request fn**, on the blocking-pool thread.
///
/// Ownership: `event_json` is plugin-produced, so it is reclaimed here with the
/// plugin's registered `plugin_free_string` — never with the host's allocator.
/// A missing context, a malformed envelope, or a panic in the host's own parsing
/// is reported as a nonzero rc; the plugin's string is still reclaimed exactly
/// once on every path, so a rejected chunk cannot leak.
extern "C" fn stream_emit_trampoline(event_json: StbString, user_data: *mut c_void) -> i32 {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if user_data.is_null() {
            // Invalid ABI invocation: there is no bridge to tell us which
            // deallocator owns this allocation. Never guess — logging is all we
            // can safely do without risking a cross-allocator free.
            tracing::error!("plugin provider emit callback missing its context");
            return 2;
        }
        // SAFETY: the blocking task owns this bridge for the entire plugin
        // request, including after the async driver returns on cancellation.
        let bridge = unsafe { &*(user_data as *const StreamEmitBridge) };
        // SAFETY: the plugin transfers a live, plugin-owned allocation exactly
        // once; `plugin_free_string` is that plugin's deallocator.
        let owned = unsafe { OwnedString::from_raw(event_json, Some(bridge.plugin_free_string)) };
        let text = owned.as_str_lossy();
        let bytes = text.len();
        let parsed = serde_json::from_str::<serde_json::Value>(&text);
        drop(text);
        drop(owned);
        match parsed {
            Ok(chunk) if chunk.is_object() => {
                // A cancelled run still needs the structural frames that close
                // the message it already showed, but has no consumer left for
                // model output. Rejecting content here is what tells a plugin to
                // stop producing instead of burning a full generation.
                let is_content = matches!(
                    chunk.get("type").and_then(serde_json::Value::as_str),
                    Some("text_delta") | Some("thinking_delta") | Some("toolcall_delta")
                );
                if bridge
                    .control
                    .as_ref()
                    .is_some_and(|state| state.is_cancelled())
                {
                    // After the aborted terminal, acknowledge cleanup frames
                    // without enqueueing events that no longer have a consumer.
                    return i32::from(is_content);
                }
                if bridge.tx.send(chunk, bytes).is_err() {
                    // Cancellation can also arrive during the capacity wait.
                    return if bridge
                        .control
                        .as_ref()
                        .is_some_and(|state| state.is_cancelled())
                    {
                        i32::from(is_content)
                    } else {
                        1
                    };
                }
                0
            }
            Ok(_) => {
                tracing::warn!("plugin provider emitted a non-object chunk — dropped");
                1
            }
            Err(error) => {
                tracing::warn!(%error, "plugin provider emitted unparseable JSON — dropped");
                1
            }
        }
    }));
    match outcome {
        Ok(rc) => rc,
        Err(_) => {
            tracing::error!("plugin provider emit callback panicked — chunk dropped");
            2
        }
    }
}

/// Run the streaming request fn inside `catch_unwind`, returning the plugin's
/// final message (or an error terminal). Mirrors
/// [`run_provider_request_inner`]'s contract: a null/empty `out`, a nonzero rc,
/// or an unparseable response all become an error terminal rather than a crash.
fn run_streaming_provider_request(
    snapshot: &Arc<RegistrySnapshot>,
    record: &RegisteredProvider,
    stream_fn: rpi_plugin_sdk::ProviderStreamRequestFn,
    bridge: &StreamEmitBridge,
    owner: &ProviderControlOwner,
    request_json: &str,
    model_id: &str,
) -> StreamingProviderOutcome {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !snapshot.is_active() {
            return Err("extensions provider registry is stale".to_string());
        }
        let req_ref = StbStringRef::from_str(request_json);
        let mut out = StbString::empty();
        let relay = bridge as *const StreamEmitBridge as *mut std::ffi::c_void;
        // `user_data` for the control callbacks is the owner's own address: the
        // trampolines cast it back, and the `Arc` keeps it alive for the call.
        let control = rpi_plugin_sdk::ProviderStreamControl {
            is_cancelled: control_is_cancelled,
            wait_cancelled_ms: control_wait_cancelled_ms,
            user_data: owner as *const ProviderControlOwner as *mut std::ffi::c_void,
        };
        let rc = stream_fn(
            req_ref,
            Some(stream_emit_trampoline),
            relay,
            &control as *const rpi_plugin_sdk::ProviderStreamControl,
            // No deferred-completion channel on this host: a plugin must finish
            // inside this call (see `ProviderStreamCompletionFn`).
            None,
            std::ptr::null_mut(),
            &mut out as *mut StbString,
            record.user_data,
        );
        // SAFETY: the callback transfers its live output to its registered
        // producer, on every rc path (including an empty slot).
        let out = unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(record.plugin_free_string))
        };
        if rc != 0 {
            // A cancelled run makes the plugin return early; that is the
            // expected outcome, not a provider failure. The driver turns it into
            // an `Aborted` terminal, so it is reported as a cancellation here.
            if bridge
                .control
                .as_ref()
                .is_some_and(|state| state.is_cancelled())
            {
                return Err(CANCELLED_MARKER.to_string());
            }
            return Err(format!(
                "plugin provider {} streaming request returned error code {rc}",
                record.provider_id
            ));
        }
        let response_text = out.as_str_lossy();
        serde_json::from_str::<AssistantMessage>(&response_text).map_err(|err| {
            format!(
                "plugin provider {} returned unparseable response: {err}",
                record.provider_id
            )
        })
    }));

    match outcome {
        Ok(Ok(mut message)) => {
            if message.provider.is_empty() {
                message.provider = record.provider_id.clone();
            }
            if message.model.is_empty() {
                message.model = model_id.to_string();
            }
            message
        }
        Ok(Err(msg)) if msg == CANCELLED_MARKER => cancelled_message(record),
        Ok(Err(msg)) => error_message(record, &msg),
        Err(_) => {
            tracing::error!(
                "plugin provider {} panicked — refusing to unwind across FFI",
                record.provider_id
            );
            error_message(
                record,
                &format!(
                    "plugin provider {} panicked during request",
                    record.provider_id
                ),
            )
        }
    }
}

/// Sentinel the blocking request runner returns so the driver can tell "the run
/// was cancelled" from "the plugin failed". It cannot collide with a real plugin
/// error: plugin-side failures are formatted as `plugin provider {id} …`, and
/// this marker starts with a NUL no plugin-produced text goes through.
const CANCELLED_MARKER: &str = "\u{0}rpi:cancelled";

/// The terminal message for a request whose caller went away. `StopReason::Aborted`
/// is what the built-in providers use, and the crate's `push_terminal` maps it to
/// `AssistantMessageEvent::Error { reason: Aborted }` — so a plugin provider is
/// indistinguishable from a built-in one in the aborted case.
fn cancelled_message(record: &RegisteredProvider) -> AssistantMessage {
    AssistantMessage::terminal(
        api_from_style(&record.api_style),
        record.provider_id.clone(),
        String::new(),
        rpi_ai::types::StopReason::Aborted,
        "plugin provider request was cancelled",
        0,
    )
}

/// An error terminal for a plugin provider. The api string is decoded exactly
/// like a deserialized `Api` (an unknown style becomes `Api::Other`), so an error
/// message from a plugin provider carries the same `api` a success would.
fn error_message(record: &RegisteredProvider, message: &str) -> AssistantMessage {
    AssistantMessage::terminal(
        api_from_style(&record.api_style),
        record.provider_id.clone(),
        String::new(),
        rpi_ai::types::StopReason::Error,
        message.to_string(),
        0,
    )
}

/// Decode the registration's `api_style` into an [`Api`](rpi_ai::types::Api).
/// Mirrors `Api`'s own `Deserialize` (including the `Other(..)` fallback) without
/// needing a throwaway serde pass.
fn api_from_style(style: &str) -> rpi_ai::types::Api {
    serde_json::from_value::<rpi_ai::types::Api>(serde_json::Value::String(style.to_string()))
        .unwrap_or_else(|_| rpi_ai::types::Api::Other(style.to_string()))
}

/// Build the `{ model, context, options, capabilities }` request envelope
/// shared by the one-shot and streaming provider paths. `Model`/`Context` are
/// Serialize; `SimpleStreamOptions` is not, so the `options` object is
/// hand-built (mirrors `ExtensionProviderHooks::before_request`).
///
/// `capabilities` names host behavior a plugin must not assume from the ABI
/// version alone. `recordedToolCalls` is the highest `recorded` version this
/// host decodes (every version from 1 up to it) and never executes. A host
/// without the key ignores `recorded` and runs the call, so a plugin must not
/// send `recorded` unless the value is at least 1.
fn request_envelope_json(model: &Model, ctx: &Context, opts: &SimpleStreamOptions) -> String {
    serde_json::json!({
        "model": serde_json::to_value(model).unwrap_or(serde_json::Value::Null),
        "context": serde_json::to_value(ctx).unwrap_or(serde_json::Value::Null),
        "options": options_json(opts),
        "capabilities": { "recordedToolCalls": 1 },
    })
    .to_string()
}

/// Live accumulator for the chunks a streaming provider emits.
///
/// The mapper owns the growing `AssistantMessage` the deltas are folded into, so
/// every `*_delta` event can carry a `partial` snapshot without the plugin having
/// to send one — the same shape the built-in providers produce (an
/// `Arc<AssistantMessage>` per event). The input queue may combine consecutive
/// pure append deltas so the mapper clones once per published batch instead of
/// once per tiny plugin callback. Supplied snapshots are always kept separate.
struct StreamMapper {
    output: AssistantMessage,
    /// Whether the stream has been opened by a `Start` event. The host opens it
    /// lazily on the first chunk so a plugin that emits nothing is never
    /// preceded by a stray `Start`.
    started: bool,
    /// Whether the terminal event has been produced; a post-terminal chunk is
    /// dropped (mirrors the provider-side "no events after terminal" rule).
    terminal: bool,
    /// Raw tool-call JSON accumulated per content index, for the same reason the
    /// Anthropic mapper keeps a `partial_json` buffer: the `arguments` object a
    /// `ToolCallEnd` carries is the *authoritative* parse, and intermediate
    /// parses must never produce a value the cumulative JSON cannot justify.
    tool_json: std::collections::BTreeMap<usize, String>,
    /// A chunk parked because the host's `Start` had to be delivered first. Set
    /// when a chunk arrives before the stream is open; re-queued by the driver on
    /// the next iteration so no delta is lost or reordered.
    deferred: Option<serde_json::Value>,
    /// The plugin's own final message, once it has returned. `Some` only after
    /// [`Self::chunks_then_message`].
    final_message: Option<AssistantMessage>,
    /// Every call a `toolcall_end` reported as already run upstream, kept so
    /// [`Self::reconcile_recorded`] can hold the final message to it.
    recorded_calls: Vec<ToolCall>,
}

impl StreamMapper {
    fn new(api: rpi_ai::types::Api, provider: String, model: String) -> Self {
        Self {
            output: AssistantMessage::empty(api, provider, model, 0),
            started: false,
            terminal: false,
            tool_json: std::collections::BTreeMap::new(),
            deferred: None,
            final_message: None,
            recorded_calls: Vec::new(),
        }
    }

    /// Take the chunk parked behind the host-emitted `Start`.
    fn take_deferred(&mut self) -> Option<serde_json::Value> {
        self.deferred.take()
    }

    fn ensure_started(&mut self) -> Option<AssistantMessageEvent> {
        if self.started {
            return None;
        }
        self.started = true;
        Some(AssistantMessageEvent::Start {
            partial: Arc::new(self.output.clone()),
        })
    }

    fn partial(&self) -> Arc<AssistantMessage> {
        Arc::new(self.output.clone())
    }

    fn aborted_message(&self) -> AssistantMessage {
        let mut message = self.output.clone();
        message.stop_reason = rpi_ai::types::StopReason::Aborted;
        message.error_message = Some("plugin provider request was cancelled".into());
        message
    }

    /// A supplied delta snapshot already includes that delta. Replace the
    /// accumulator without appending the same text/arguments a second time.
    fn apply_partial(&mut self, partial: Option<serde_json::Value>) -> bool {
        match partial.map(serde_json::from_value::<AssistantMessage>) {
            Some(Ok(message)) => {
                self.output = message;
                true
            }
            Some(Err(error)) => {
                tracing::warn!(%error, "ignoring malformed plugin provider partial snapshot");
                false
            }
            None => false,
        }
    }

    /// Fold one plugin chunk into the accumulator and return the event it maps
    /// to. `None` means "nothing to emit" (an unknown `type`, or a malformed
    /// chunk the plugin is allowed to get wrong without killing the run).
    fn apply(&mut self, chunk: serde_json::Value) -> Option<AssistantMessageEvent> {
        if self.terminal {
            tracing::debug!("dropping a plugin provider chunk that follows a terminal event");
            return None;
        }
        let mut chunk = chunk;
        chunk.get("type").and_then(serde_json::Value::as_str)?;

        // A chunk that opens or continues a block implies the stream is open.
        // `Start` is emitted first so the consumer's `MessageStart` never lands
        // after a delta (the agent loop and the frame encoder both require the
        // ordering). The chunk is parked and re-applied by `drive_...`'s next
        // iteration, so no delta is lost or reordered.
        if !self.started {
            self.started = true;
            self.deferred = Some(std::mem::replace(&mut chunk, serde_json::Value::Null));
            return Some(AssistantMessageEvent::Start {
                partial: self.partial(),
            });
        }

        // Extract only after deferral: the parked frame must keep its snapshot.
        let partial = chunk.as_object_mut()?.remove("partial");
        let kind = chunk.get("type").and_then(serde_json::Value::as_str)?;
        match kind {
            // `start` is not an event of its own: the host emits `Start` above
            // the first time it sees *any* chunk, so a duplicate is dropped.
            "start" => None,
            "usage" => {
                // Usage is not an event of its own: it patches the accumulating
                // message so the terminal one reports it. A plugin that sends no
                // usage keeps the zeroed default.
                if let Some(usage) = chunk.get("usage") {
                    if let Ok(parsed) =
                        serde_json::from_value::<rpi_ai::types::Usage>(usage.clone())
                    {
                        self.output.usage = parsed;
                    }
                }
                None
            }
            "text_start" => {
                let index = self.content_index(&chunk);
                self.open_block(index, Content::text(String::new()));
                Some(AssistantMessageEvent::TextStart {
                    content_index: index,
                    partial: self.partial(),
                })
            }
            "text_delta" => {
                let index = self.content_index(&chunk);
                let delta = take_chunk_string(&mut chunk, "delta");
                if !self.apply_partial(partial) {
                    let block = self.ensure_text_block(index);
                    block.text.push_str(&delta);
                }
                Some(AssistantMessageEvent::TextDelta {
                    content_index: index,
                    delta,
                    partial: self.partial(),
                })
            }
            "text_end" => {
                let index = self.content_index(&chunk);
                let block = self.ensure_text_block(index);
                let content = if chunk.get("content").is_some() {
                    chunk_string(&chunk, "content")
                } else {
                    block.text.clone()
                };
                block.text = content.clone();
                Some(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content,
                    partial: self.partial(),
                })
            }
            "thinking_start" => {
                let index = self.content_index(&chunk);
                self.open_block(index, Content::thinking(String::new()));
                Some(AssistantMessageEvent::ThinkingStart {
                    content_index: index,
                    partial: self.partial(),
                })
            }
            "thinking_delta" => {
                let index = self.content_index(&chunk);
                let delta = take_chunk_string(&mut chunk, "delta");
                if !self.apply_partial(partial) {
                    let block = self.ensure_thinking_block(index);
                    block.thinking.push_str(&delta);
                }
                Some(AssistantMessageEvent::ThinkingDelta {
                    content_index: index,
                    delta,
                    partial: self.partial(),
                })
            }
            "thinking_end" => {
                let index = self.content_index(&chunk);
                let block = self.ensure_thinking_block(index);
                let content = if chunk.get("content").is_some() {
                    chunk_string(&chunk, "content")
                } else {
                    block.thinking.clone()
                };
                block.thinking = content.clone();
                let redacted = chunk
                    .get("redacted")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                block.redacted = redacted;
                Some(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content,
                    partial: self.partial(),
                })
            }
            "toolcall_start" => {
                let index = self.content_index(&chunk);
                let id = chunk_string(&chunk, "id");
                let name = chunk_string(&chunk, "name");
                // The `arguments` snapshot at start is authoritative for the
                // arguments the block already has; the delta stream then extends
                // it. A plugin that sends only deltas starts from an empty
                // object, which is what the frame encoder expects.
                let arguments = chunk
                    .get("arguments")
                    .cloned()
                    .filter(serde_json::Value::is_object)
                    .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
                self.tool_json.insert(index, String::new());
                self.open_block(index, Content::tool_call(id, name, arguments));
                Some(AssistantMessageEvent::ToolCallStart {
                    content_index: index,
                    partial: self.partial(),
                })
            }
            "toolcall_delta" => {
                let index = self.content_index(&chunk);
                let delta = take_chunk_string(&mut chunk, "delta");
                let accumulated = self.tool_json.entry(index).or_default();
                accumulated.push_str(&delta);
                // Re-parse on every delta so the partial message renders live
                // arguments (the same invariant the Anthropic mapper holds).
                let parsed = rpi_ai::json_parse::parse_streaming_json(Some(accumulated));
                if !self.apply_partial(partial) {
                    match self.output.content.get_mut(index) {
                        Some(Content::ToolCall(call)) => call.arguments = parsed,
                        _ => {
                            let id = chunk_string(&chunk, "id");
                            let name = chunk_string(&chunk, "name");
                            self.ensure_tool_call_block(index, id, name);
                            if let Some(Content::ToolCall(call)) =
                                self.output.content.get_mut(index)
                            {
                                call.arguments = parsed;
                            }
                        }
                    }
                }
                Some(AssistantMessageEvent::ToolCallDelta {
                    content_index: index,
                    delta,
                    partial: self.partial(),
                })
            }
            "toolcall_end" => {
                let index = self.content_index(&chunk);
                let id = chunk_string(&chunk, "id");
                let name = chunk_string(&chunk, "name");
                // Presence, not truthiness: an explicit `null` is a malformed
                // recording and must reach the agent loop as one.
                let recorded = chunk.get("recorded").cloned();
                let arguments = chunk
                    .get("arguments")
                    .cloned()
                    .filter(serde_json::Value::is_object)
                    .or_else(|| match self.output.content.get(index) {
                        Some(Content::ToolCall(call)) => Some(call.arguments.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
                self.ensure_tool_call_block(index, id.clone(), name.clone());
                let call = match self.output.content.get_mut(index) {
                    Some(Content::ToolCall(call)) => {
                        if !id.is_empty() {
                            call.id = id.clone();
                        }
                        if !name.is_empty() {
                            call.name = name.clone();
                        }
                        call.arguments = arguments.clone();
                        call.recorded = recorded;
                        call.clone()
                    }
                    _ => ToolCall {
                        kind: ToolCallType,
                        id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                        thought_signature: None,
                        namespace: None,
                        recorded,
                    },
                };
                self.tool_json.remove(&index);
                if call.recorded.is_some() && !self.recorded_calls.contains(&call) {
                    self.recorded_calls.push(call.clone());
                }
                Some(AssistantMessageEvent::ToolCallEnd {
                    content_index: index,
                    tool_call: call,
                    partial: self.partial(),
                })
            }
            other => {
                tracing::debug!(
                    chunk_type = other,
                    "ignoring an unknown plugin provider chunk"
                );
                None
            }
        }
    }

    fn content_index(&self, chunk: &serde_json::Value) -> usize {
        chunk
            .get("contentIndex")
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(0)
    }

    /// Append a freshly-opened block at `index`, padding any gap with empty text
    /// blocks so the index the plugin named is the index the message has. A
    /// plugin that names indices out of order is a plugin bug, but it must not
    /// corrupt the message the host persists.
    fn open_block(&mut self, index: usize, content: Content) {
        while self.output.content.len() <= index {
            self.output.content.push(Content::text(String::new()));
        }
        self.output.content[index] = content;
    }

    fn ensure_text_block(&mut self, index: usize) -> &mut rpi_ai::types::TextContent {
        self.ensure_block(index, || Content::text(String::new()));
        match self.output.content.get_mut(index) {
            Some(Content::Text(block)) => block,
            other => unreachable!("ensure_text_block left a non-text block: {other:?}"),
        }
    }

    fn ensure_thinking_block(&mut self, index: usize) -> &mut rpi_ai::types::ThinkingContent {
        self.ensure_block(index, || Content::thinking(String::new()));
        match self.output.content.get_mut(index) {
            Some(Content::Thinking(block)) => block,
            other => unreachable!("ensure_thinking_block left a non-thinking block: {other:?}"),
        }
    }

    fn ensure_tool_call_block(&mut self, index: usize, id: String, name: String) {
        self.ensure_block(index, || {
            Content::tool_call(
                id.clone(),
                name.clone(),
                serde_json::Value::Object(serde_json::Map::new()),
            )
        });
    }

    fn ensure_block(&mut self, index: usize, make: impl FnOnce() -> Content) {
        if self.output.content.len() <= index {
            // No `*_start` opened this block: the plugin skipped it. Repair the
            // message (the deltas still land) but say so — a missing start is a
            // contract violation the plugin author should see in the logs rather
            // than a silently different message shape.
            tracing::warn!(
                content_index = index,
                "plugin provider sent a block delta without its start chunk — synthesizing the block"
            );
            while self.output.content.len() < index {
                self.output.content.push(Content::text(String::new()));
            }
            self.output.content.push(make());
        }
    }

    /// Hold the plugin's final `message` to every recording a `toolcall_end`
    /// reported. A final call with the same id, name and arguments and no
    /// `recorded` key gets the observed recording, including an explicit `null`.
    /// Anything that could let a recorded call run is an error: a recorded call
    /// with no id, two recorded calls or two final calls sharing an id, a final
    /// message without the id, a different name or arguments, or a different
    /// recording. Calls never reported as recorded are left alone.
    fn reconcile_recorded(&self, message: &mut AssistantMessage) -> Result<(), &'static str> {
        for observed in &self.recorded_calls {
            let id = &observed.id;
            let reject = |reason: &'static str| {
                // The id stays out of the error text (see the driver); log it.
                tracing::warn!(tool_call_id = %id, reason, "plugin provider final message drops a recorded tool call");
                Err(reason)
            };
            if id.is_empty() {
                return reject("a recorded call has no id");
            }
            if self
                .recorded_calls
                .iter()
                .filter(|call| &call.id == id)
                .count()
                > 1
            {
                return reject("two recorded calls share an id");
            }
            let mut finals = message.content.iter_mut().filter_map(|block| match block {
                Content::ToolCall(call) if &call.id == id => Some(call),
                _ => None,
            });
            let Some(call) = finals.next() else {
                return reject("the call is missing");
            };
            if finals.next().is_some() {
                return reject("the call is repeated");
            }
            if call.name != observed.name || call.arguments != observed.arguments {
                return reject("its name or arguments changed");
            }
            match &call.recorded {
                None => call.recorded = observed.recorded.clone(),
                Some(recorded) if Some(recorded) == observed.recorded.as_ref() => {}
                Some(_) => return reject("its recording differs"),
            }
        }
        Ok(())
    }

    /// Events that must precede the terminal event for `message`: whatever the
    /// mapper has not yet delivered (at minimum a `Start`, so a plugin that
    /// emitted nothing still produces a well-formed stream).
    /// `message` is the plugin's final payload on completion, or the accumulated
    /// partial with an Aborted stop reason when the caller cancels.
    fn chunks_then_message(&mut self, message: &AssistantMessage) -> Vec<AssistantMessageEvent> {
        let mut events = Vec::new();
        if let Some(start) = self.ensure_started() {
            events.push(start);
        }
        self.terminal = true;
        self.final_message = Some(message.clone());
        events
    }

    /// The accumulated message, for tests and diagnostics.
    #[cfg(test)]
    fn accumulated(&self) -> &AssistantMessage {
        &self.output
    }
}

/// Read a string field from a chunk, defaulting to empty.
fn take_chunk_string(chunk: &mut serde_json::Value, field: &str) -> String {
    match chunk
        .as_object_mut()
        .and_then(|object| object.remove(field))
    {
        Some(serde_json::Value::String(text)) => text,
        _ => String::new(),
    }
}

/// Copy a string field that must remain in the chunk, defaulting to empty.
fn chunk_string(chunk: &serde_json::Value, field: &str) -> String {
    chunk
        .get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ExtensionRegistry;

    #[test]
    fn stream_idle_duration_preserves_disabled_default_and_large_values_across_abi() {
        for (duration, expected) in [
            (None, serde_json::Value::Null),
            (Some(std::time::Duration::ZERO), serde_json::json!(0)),
            (
                Some(std::time::Duration::from_secs(45)),
                serde_json::json!(45_000),
            ),
            // A valid u64 CLI seconds value must not wrap to a tiny timeout.
            (
                Some(std::time::Duration::from_secs(u64::MAX / 1000 + 1)),
                serde_json::json!(u64::MAX),
            ),
        ] {
            let options = SimpleStreamOptions {
                stream_idle_timeout: duration,
                ..Default::default()
            };
            assert_eq!(options_json(&options)["streamIdleTimeoutMs"], expected);
        }
    }

    rpi_plugin_sdk::string_handler! {
        /// A stub `ProviderRequestFn` that writes a minimal assistant-message JSON
        /// into `out`. Mirrors the shape a real plugin's request fn returns.
        fn ok_request_fn(_req, out, _ud) {
        let json = r#"{"role":"assistant","content":[{"type":"text","text":"hi"}],"api":"faux","provider":"pluggy","model":"","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"stopReason":"stop","timestamp":0}"#;
        i32::from(!out.write(json.to_string()))
    } }

    /// A free fn matching the stub's allocation (`StbString::from_string` ⇒ a
    /// `Box<[u8]>` reclaimed by reconstructing the slice).
    use rpi_plugin_sdk::callbacks::free_string as stub_free;

    /// A v2 "provider" whose synchronous request body blocks the way a real one
    /// would (it waits for cancellation after an initial burst) and reports what
    /// it saw. Used to drive the whole cancellation path in-process, without the
    /// cdylib, so the decision logic is covered by a fast unit test too.
    mod cancel_provider {
        use super::*;
        use rpi_plugin_sdk::{ProviderStreamCompletionFn, ProviderStreamControl};
        use std::sync::atomic::AtomicUsize;

        pub static EMITTED: AtomicUsize = AtomicUsize::new(0);
        pub static OBSERVED: AtomicUsize = AtomicUsize::new(0);

        pub extern "C" fn request_v2(
            _req: StbStringRef,
            emit: Option<rpi_plugin_sdk::ProviderStreamEmitFn>,
            emit_user_data: *mut c_void,
            control: *const ProviderStreamControl,
            _completion: Option<ProviderStreamCompletionFn>,
            _completion_user_data: *mut c_void,
            out: *mut StbString,
            _ud: *mut c_void,
        ) -> i32 {
            let control = unsafe { *control };
            if let Some(emit) = emit {
                let start = r#"{"type":"text_start","contentIndex":0}"#;
                let _ = emit(StbString::from_string(start.to_string()), emit_user_data);
                for i in 0..50 {
                    // Block on the host's control surface, exactly as a provider
                    // parked in an SSE read would.
                    if (control.wait_cancelled_ms)(control.user_data, 20) != 0 {
                        OBSERVED.fetch_add(1, Ordering::SeqCst);
                        return 1;
                    }
                    let chunk =
                        format!(r#"{{"type":"text_delta","contentIndex":0,"delta":"{i},"}}"#);
                    if emit(StbString::from_string(chunk), emit_user_data) != 0 {
                        OBSERVED.fetch_add(1, Ordering::SeqCst);
                        EMITTED.fetch_add(1, Ordering::SeqCst);
                        return 1;
                    }
                    EMITTED.fetch_add(1, Ordering::SeqCst);
                }
            }
            let json = r#"{"role":"assistant","content":[{"type":"text","text":"completed"}],"api":"faux","provider":"cancel-probe","model":"m","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}},"stopReason":"stop","timestamp":0}"#;
            unsafe {
                *out = StbString::from_string(json.to_string());
            }
            0
        }
    }

    /// `run_provider_request_inner` round-trips a nonzero-rc plugin response to
    /// `Err`, and a valid-message response to `Ok` (with provider stamped from
    /// the record when the plugin omits it).
    #[test]
    fn request_fn_round_trip_ok_and_err() {
        let mut registry = ExtensionRegistry::new();
        registry.register_provider(RegisteredProvider {
            provider_id: "pluggy".to_string(),
            base_url: "https://example".to_string(),
            api_style: "anthropic-messages".to_string(),
            request_fn: ok_request_fn,
            stream_fn: None,
            plugin_free_string: stub_free,
            user_data: std::ptr::null_mut(),
        });
        let snap = Arc::new(registry.snapshot());
        let record = snap.providers()[0].clone();

        // Valid response → Ok(message).
        let outcome = run_provider_request(&record, r#"{"model":"m"}"#, "m");
        match outcome {
            ProviderOutcome::Ok(m) => {
                assert_eq!(m.provider, "pluggy");
                assert_eq!(m.model, "m");
                assert_eq!(m.stop_reason, rpi_ai::types::StopReason::Stop);
                assert_eq!(m.content.len(), 1);
            }
            _ => panic!("expected Ok"),
        }

        // A request fn that returns nonzero rc ⇒ Err (registered separately).
        extern "C" fn err_request_fn(
            _req: StbStringRef,
            _out: *mut StbString,
            _ud: *mut std::ffi::c_void,
        ) -> i32 {
            7
        }
        let err_record = RegisteredProvider {
            provider_id: "pluggy".to_string(),
            base_url: "https://example".to_string(),
            api_style: "anthropic-messages".to_string(),
            request_fn: err_request_fn,
            stream_fn: None,
            plugin_free_string: stub_free,
            user_data: std::ptr::null_mut(),
        };
        let outcome = run_provider_request(&err_record, r#"{"model":"m"}"#, "m");
        assert!(matches!(outcome, ProviderOutcome::Err(_)));
    }

    /// A streaming provider's deltas must reach the event stream **in order**, as
    /// `TextStart` → `TextDelta`×N → `TextEnd`, all before the terminal `Done`.
    /// This is the property the whole feature exists for: the consumer renders
    /// while the plugin is still producing.
    #[test]
    fn stream_chunks_map_to_ordered_events() {
        let mut mapper = StreamMapper::new(rpi_ai::types::Api::Faux, "pluggy".into(), "m".into());
        let chunks = [
            serde_json::json!({"type":"start"}),
            serde_json::json!({"type":"text_start","contentIndex":0}),
            serde_json::json!({"type":"text_delta","contentIndex":0,"delta":"hel"}),
            serde_json::json!({"type":"text_delta","contentIndex":0,"delta":"lo"}),
            serde_json::json!({"type":"text_end","contentIndex":0}),
        ];
        let mut tags = Vec::new();
        for chunk in chunks {
            if let Some(event) = mapper.apply(chunk) {
                tags.push(event.type_tag());
            }
            while let Some(deferred) = mapper.take_deferred() {
                if let Some(event) = mapper.apply(deferred) {
                    tags.push(event.type_tag());
                }
            }
        }
        assert_eq!(
            tags,
            vec![
                "start",
                "text_start",
                "text_delta",
                "text_delta",
                "text_end"
            ],
            "chunks must map to an ordered, non-terminal event sequence"
        );
        assert_eq!(
            mapper.accumulated().content[0],
            rpi_ai::types::Content::text("hello"),
            "deltas accumulate into the block they name"
        );
    }

    /// The aborted terminal must map onto `ErrorReason::Aborted`, not `Error`:
    /// a user-cancelled run is not a provider fault, and the agent loop and TUI
    /// report the two differently.
    #[test]
    fn an_aborted_message_terminates_with_aborted_not_error() {
        let (mut prod, mut stream) = create_assistant_message_event_stream();
        let record = RegisteredProvider {
            provider_id: "pluggy".to_string(),
            base_url: "https://example".to_string(),
            api_style: "faux".to_string(),
            request_fn: ok_request_fn,
            stream_fn: None,
            plugin_free_string: stub_free,
            user_data: std::ptr::null_mut(),
        };
        futures::executor::block_on(push_terminal(&mut prod, cancelled_message(&record)));
        let event = futures::executor::block_on(stream.next()).expect("a terminal event");
        match event {
            AssistantMessageEvent::Error { reason, error } => {
                assert_eq!(reason, ErrorReason::Aborted);
                assert_eq!(error.stop_reason, rpi_ai::types::StopReason::Aborted);
            }
            other => panic!("expected Aborted Error, got {other:?}"),
        }
    }

    /// A cancelled v2 request terminates on `Aborted` promptly, and the plugin's
    /// control surface reports it — the end-to-end property the cancel protocol
    /// exists for, driven in-process rather than through the cdylib.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_v2_request_terminates_aborted_and_tells_the_plugin() {
        cancel_provider::EMITTED.store(0, Ordering::SeqCst);
        cancel_provider::OBSERVED.store(0, Ordering::SeqCst);

        let record = RegisteredProvider {
            provider_id: "cancel-probe".to_string(),
            base_url: "https://example".to_string(),
            api_style: "faux".to_string(),
            request_fn: ok_request_fn,
            stream_fn: Some(cancel_provider::request_v2),
            plugin_free_string: stub_free,
            user_data: std::ptr::null_mut(),
        };
        let mut registry = ExtensionRegistry::new();
        registry.register_streaming_provider(record.clone());
        let snapshot = Arc::new(registry.snapshot());

        let model = Model::new(
            "m",
            "M",
            rpi_ai::types::Api::Faux,
            "cancel-probe",
            "https://example",
        );
        let ctx = Context::new(Vec::new());
        let signal = tokio_util::sync::CancellationToken::new();
        let opts = SimpleStreamOptions {
            signal: signal.clone(),
            ..SimpleStreamOptions::default()
        };

        let (mut prod, mut stream) = create_assistant_message_event_stream();
        // The driver returns the terminal message; the derived event lands on the
        // producer, so read the stream for the events the consumer would see.
        tokio::spawn(async move {
            let stream_fn = record.stream_fn.expect("a streaming fn");
            let message = drive_streaming_plugin_provider(
                &snapshot,
                &record,
                stream_fn,
                Handle::current(),
                ProviderRequestLifetime::new(PluginKeepalive::empty()),
                &model,
                &ctx,
                &opts,
                &mut prod,
            )
            .await;
            push_terminal(&mut prod, message).await;
        });

        let mut aborted = false;
        let mut saw_delta = false;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while let Some(event) = stream.next().await {
            match &event {
                AssistantMessageEvent::TextDelta { .. } if !saw_delta => {
                    saw_delta = true;
                    signal.cancel();
                }
                AssistantMessageEvent::Error { reason, .. } => {
                    aborted = *reason == ErrorReason::Aborted;
                }
                _ => {}
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a cancelled request must terminate promptly"
            );
        }

        assert!(saw_delta, "the provider must have started producing");
        assert!(aborted, "the terminal must be Aborted");
        // The host terminal no longer waits for the plugin to unwind.
        tokio::time::timeout(Duration::from_secs(2), async {
            while cancel_provider::OBSERVED.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the plugin observes cancellation eventually");
        assert_eq!(
            cancel_provider::OBSERVED.load(Ordering::SeqCst),
            1,
            "the plugin must have observed the cancellation through its control surface"
        );
        assert!(
            cancel_provider::EMITTED.load(Ordering::SeqCst) < 50,
            "the plugin must stop before its 50-delta budget"
        );
    }

    /// A plugin that ignores the control surface still terminates correctly: the
    /// host stops forwarding content chunks once cancelled, so `emit` reports
    /// rejection and the run ends on `Aborted` rather than hanging.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_request_stops_forwarding_content_even_if_the_plugin_pushes() {
        let owner = Arc::new(ProviderControlOwner {
            state: Arc::new(ProviderControlState::default()),
        });
        let (tx, mut rx) = queue::channel(64, 4 * 1024 * 1024, Arc::clone(&owner.state));
        let bridge = StreamEmitBridge {
            tx,
            plugin_free_string: stub_free,
            control: Some(Arc::clone(&owner.state)),
        };
        let relay = &bridge as *const StreamEmitBridge as *mut c_void;

        // Not yet cancelled: a content chunk is forwarded.
        let delta = r#"{"type":"text_delta","contentIndex":0,"delta":"a"}"#;
        assert_eq!(
            stream_emit_trampoline(StbString::from_string(delta.to_string()), relay),
            0
        );
        assert!(rx.try_recv().is_ok(), "an un-cancelled chunk is delivered");

        owner.state.cancel();

        // Cancelled: the same chunk is rejected, and a structural one still lands
        // so the message can be closed.
        assert_eq!(
            stream_emit_trampoline(StbString::from_string(delta.to_string()), relay),
            1,
            "a content chunk is refused once the run is cancelled"
        );
        let end = r#"{"type":"text_end","contentIndex":0,"content":"a"}"#;
        assert_eq!(
            stream_emit_trampoline(StbString::from_string(end.to_string()), relay),
            0,
            "a structural chunk stays admissible while winding down"
        );
        assert!(
            rx.try_recv().is_err(),
            "cleanup must not produce post-terminal events"
        );
    }

    /// The control surface a v2 plugin polls: `is_cancelled` reports the latched
    /// flag for content chunks but never for structural ones (a cancelled run
    /// still needs the frames that close the message it already showed), and
    /// `wait_cancelled_ms` returns promptly once the flag is set.
    #[test]
    fn the_control_surface_reports_cancellation_for_content_only() {
        let owner = Arc::new(ProviderControlOwner {
            state: Arc::new(ProviderControlState::default()),
        });
        let user_data = Arc::as_ptr(&owner) as *mut c_void;
        let content = rpi_plugin_sdk::ProviderChunkKind::Content as u32;
        let structural = rpi_plugin_sdk::ProviderChunkKind::Structural as u32;

        assert_eq!(control_is_cancelled(user_data, content), 0);
        assert_eq!(control_wait_cancelled_ms(user_data, 0), 0);

        owner.state.cancel();

        assert_eq!(
            control_is_cancelled(user_data, content),
            1,
            "content chunks must report cancelled"
        );
        assert_eq!(
            control_is_cancelled(user_data, structural),
            0,
            "structural chunks stay admissible while a cancelled request winds down"
        );
        // A blocking wait returns as soon as the (already-set) flag is seen.
        let started = std::time::Instant::now();
        assert_eq!(control_wait_cancelled_ms(user_data, 60_000), 1);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "an already-cancelled wait must not sleep out its timeout"
        );
        // A null `user_data` is the defensive floor: never report cancelled, so a
        // plugin keeps its normal behavior instead of aborting.
        assert_eq!(control_is_cancelled(std::ptr::null_mut(), content), 0);
        assert_eq!(control_wait_cancelled_ms(std::ptr::null_mut(), 0), 0);
    }

    /// A plugin that emits a delta with no preceding `start` still gets a
    /// well-formed stream: the host inserts `Start` first and re-applies the
    /// delta, so the frame encoder's `before start` rule is never violated.
    #[test]
    fn a_delta_before_start_still_opens_the_stream() {
        let mut mapper = StreamMapper::new(rpi_ai::types::Api::Faux, "pluggy".into(), "m".into());
        let first = mapper
            .apply(serde_json::json!({"type":"text_delta","contentIndex":0,"delta":"x"}))
            .expect("a start event");
        assert_eq!(first.type_tag(), "start");
        let deferred = mapper.take_deferred().expect("the parked delta");
        let second = mapper.apply(deferred).expect("the delta event");
        assert_eq!(second.type_tag(), "text_delta");
    }

    /// Tool-call deltas accumulate raw JSON and the end event carries the parsed
    /// object — the same `partial_json` invariant the Anthropic mapper holds, so
    /// a `ToolCallEnd` never contains unparseable arguments.
    #[test]
    fn tool_call_deltas_accumulate_json() {
        let mut mapper = StreamMapper::new(rpi_ai::types::Api::Faux, "pluggy".into(), "m".into());
        let mut tags = Vec::new();
        for chunk in [
            serde_json::json!({"type":"toolcall_start","contentIndex":0,"id":"c1","name":"echo"}),
            serde_json::json!({"type":"toolcall_delta","contentIndex":0,"delta":"{\"text\":"}),
            serde_json::json!({"type":"toolcall_delta","contentIndex":0,"delta":"\"hi\"}"}),
        ] {
            if let Some(event) = mapper.apply(chunk) {
                tags.push(event.type_tag());
            }
            while let Some(deferred) = mapper.take_deferred() {
                if let Some(event) = mapper.apply(deferred) {
                    tags.push(event.type_tag());
                }
            }
        }
        // The first `toolcall_start` arrives before the stream is open, so the
        // host's `start` goes out first and the chunk is re-applied right after.
        assert_eq!(
            tags,
            vec![
                "start",
                "toolcall_start",
                "toolcall_delta",
                "toolcall_delta"
            ]
        );
        let event = mapper
            .apply(
                serde_json::json!({"type":"toolcall_end","contentIndex":0,"id":"c1","name":"echo"}),
            )
            .expect("tool call end");
        let AssistantMessageEvent::ToolCallEnd { tool_call, .. } = event else {
            panic!("expected ToolCallEnd");
        };
        assert_eq!(tool_call.name, "echo");
        assert_eq!(tool_call.arguments["text"], "hi");
    }

    /// A plugin replaying a call the remote agent already ran must reach the
    /// host with its recording intact: dropping it would let the local loop run
    /// the call a second time.
    #[test]
    fn tool_call_end_keeps_the_recording_of_an_already_executed_call() {
        let recording = serde_json::json!({
            "version": 1,
            "isError": false,
            "result": {
                "content": [{ "type": "text", "text": "recorded output" }],
                "details": {},
                "terminate": true
            }
        });
        let mut mapper = StreamMapper::new(rpi_ai::types::Api::Faux, "pluggy".into(), "m".into());
        let mut ends = Vec::new();
        for chunk in [
            serde_json::json!({"type":"toolcall_start","contentIndex":0,"id":"c1","name":"shell"}),
            serde_json::json!({"type":"toolcall_end","contentIndex":0,"id":"c1","name":"shell","arguments":{"command":"rm -rf target"},"recorded":recording}),
        ] {
            if let Some(AssistantMessageEvent::ToolCallEnd { tool_call, .. }) = mapper.apply(chunk)
            {
                ends.push(tool_call);
            }
            while let Some(deferred) = mapper.take_deferred() {
                if let Some(AssistantMessageEvent::ToolCallEnd { tool_call, .. }) =
                    mapper.apply(deferred)
                {
                    ends.push(tool_call);
                }
            }
        }
        let [tool_call] = ends.as_slice() else {
            panic!("expected one ToolCallEnd, got {}", ends.len());
        };
        assert_eq!(
            serde_json::to_value(tool_call).unwrap()["recorded"],
            recording
        );
        assert_eq!(tool_call.arguments["command"], "rm -rf target");
    }

    /// A plugin written before `recorded` existed sends `toolcall_end` without
    /// the key. Its call must stay an ordinary call that the loop executes.
    #[test]
    fn tool_call_end_without_a_recording_is_an_ordinary_call() {
        let mut mapper = StreamMapper::new(rpi_ai::types::Api::Faux, "pluggy".into(), "m".into());
        let mut ends = Vec::new();
        for chunk in [
            serde_json::json!({"type":"toolcall_start","contentIndex":0,"id":"c1","name":"echo"}),
            serde_json::json!({"type":"toolcall_end","contentIndex":0,"id":"c1","name":"echo","arguments":{"text":"hi"}}),
        ] {
            if let Some(AssistantMessageEvent::ToolCallEnd { tool_call, .. }) = mapper.apply(chunk)
            {
                ends.push(tool_call);
            }
            while let Some(deferred) = mapper.take_deferred() {
                if let Some(AssistantMessageEvent::ToolCallEnd { tool_call, .. }) =
                    mapper.apply(deferred)
                {
                    ends.push(tool_call);
                }
            }
        }
        let [tool_call] = ends.as_slice() else {
            panic!("expected one ToolCallEnd, got {}", ends.len());
        };
        assert_eq!(tool_call.recorded, None);
        assert!(serde_json::to_value(tool_call)
            .unwrap()
            .get("recorded")
            .is_none());
        assert_eq!(tool_call.arguments["text"], "hi");
    }

    /// The recording a `toolcall_end` carries in the reconciliation tests.
    fn recording(text: &str) -> serde_json::Value {
        serde_json::json!({
            "version": 1,
            "isError": false,
            "result": { "content": [{ "type": "text", "text": text }] }
        })
    }

    /// A `toolcall_end` chunk for `id` at `index`, with `recorded` when given.
    fn end_chunk(index: usize, id: &str, recorded: Option<serde_json::Value>) -> serde_json::Value {
        let mut chunk = serde_json::json!({
            "type": "toolcall_end", "contentIndex": index, "id": id,
            "name": "shell", "arguments": { "command": id }
        });
        if let Some(recorded) = recorded {
            chunk["recorded"] = recorded;
        }
        chunk
    }

    /// Run `chunks` through a mapper, as the streaming driver does.
    fn mapper_after(chunks: Vec<serde_json::Value>) -> StreamMapper {
        let mut mapper = StreamMapper::new(rpi_ai::types::Api::Faux, "pluggy".into(), "m".into());
        for chunk in chunks {
            let _ = mapper.apply(chunk);
            while let Some(deferred) = mapper.take_deferred() {
                let _ = mapper.apply(deferred);
            }
        }
        mapper
    }

    /// A final `out` message holding `calls`, given as tool-call JSON.
    fn final_with(calls: Vec<serde_json::Value>) -> AssistantMessage {
        let mut message = AssistantMessage::empty(rpi_ai::types::Api::Faux, "pluggy", "m", 0);
        message.stop_reason = rpi_ai::types::StopReason::ToolUse;
        for call in calls {
            message
                .content
                .push(serde_json::from_value(call).expect("tool call decodes"));
        }
        message
    }

    /// A final tool call for `id` with the arguments `end_chunk` streamed.
    fn final_call(id: &str, recorded: Option<serde_json::Value>) -> serde_json::Value {
        let mut call = serde_json::json!({
            "type": "toolCall", "id": id, "name": "shell", "arguments": { "command": id }
        });
        if let Some(recorded) = recorded {
            call["recorded"] = recorded;
        }
        call
    }

    fn recorded_of(message: &AssistantMessage) -> Vec<Option<serde_json::Value>> {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                Content::ToolCall(call) => Some(call.recorded.clone()),
                _ => None,
            })
            .collect()
    }

    /// A recording seen only on `toolcall_end` reaches the final message, and so
    /// does an explicit `null`, which the agent loop then rejects.
    #[test]
    fn final_message_gets_a_recording_seen_only_on_the_stream() {
        for recorded in [recording("upstream"), serde_json::Value::Null] {
            let mapper = mapper_after(vec![end_chunk(0, "c1", Some(recorded.clone()))]);
            let mut message = final_with(vec![final_call("c1", None)]);
            mapper.reconcile_recorded(&mut message).unwrap();
            assert_eq!(recorded_of(&message), vec![Some(recorded)]);
        }
    }

    /// Several recorded calls next to ordinary ones: each recorded call gets its
    /// own recording, and the ordinary calls stay executable.
    #[test]
    fn only_recorded_calls_are_marked_among_mixed_calls() {
        let mapper = mapper_after(vec![
            end_chunk(0, "c1", Some(recording("one"))),
            end_chunk(1, "c2", None),
            end_chunk(2, "c3", Some(recording("three"))),
        ]);
        let mut message = final_with(vec![
            final_call("c1", None),
            final_call("c2", None),
            final_call("c3", None),
        ]);
        mapper.reconcile_recorded(&mut message).unwrap();
        assert_eq!(
            recorded_of(&message),
            vec![Some(recording("one")), None, Some(recording("three"))]
        );
    }

    /// A final message that already agrees is left as it is, and a recording
    /// present only in the final message still counts.
    #[test]
    fn agreeing_and_final_only_recordings_are_kept() {
        let mapper = mapper_after(vec![
            end_chunk(0, "c1", Some(recording("both"))),
            end_chunk(1, "c2", None),
        ]);
        let mut message = final_with(vec![
            final_call("c1", Some(recording("both"))),
            final_call("c2", Some(recording("final only"))),
        ]);
        let expected = message.clone();
        mapper.reconcile_recorded(&mut message).unwrap();
        assert_eq!(message, expected);
    }

    /// Every final message that cannot keep a streamed recording is rejected.
    #[test]
    fn a_final_message_that_drops_a_recording_is_rejected() {
        let recorded = || Some(recording("upstream"));
        let renamed = {
            let mut call = final_call("c1", None);
            call["name"] = "other".into();
            call
        };
        let new_arguments = {
            let mut call = final_call("c1", None);
            call["arguments"] = serde_json::json!({ "command": "changed" });
            call
        };
        let cases = [
            (
                "different recording",
                vec![end_chunk(0, "c1", recorded())],
                vec![final_call("c1", Some(recording("other")))],
                "its recording differs",
            ),
            (
                "null versus recording",
                vec![end_chunk(0, "c1", Some(serde_json::Value::Null))],
                vec![final_call("c1", recorded())],
                "its recording differs",
            ),
            (
                "missing id",
                vec![end_chunk(0, "c1", recorded())],
                vec![final_call("c2", None)],
                "the call is missing",
            ),
            (
                "renamed",
                vec![end_chunk(0, "c1", recorded())],
                vec![renamed],
                "its name or arguments changed",
            ),
            (
                "new arguments",
                vec![end_chunk(0, "c1", recorded())],
                vec![new_arguments],
                "its name or arguments changed",
            ),
            (
                "duplicate final id",
                vec![end_chunk(0, "c1", recorded())],
                vec![final_call("c1", None), final_call("c1", None)],
                "the call is repeated",
            ),
            (
                "duplicate streamed id",
                vec![
                    end_chunk(0, "c1", recorded()),
                    end_chunk(1, "c1", Some(recording("other"))),
                ],
                vec![final_call("c1", None)],
                "two recorded calls share an id",
            ),
            (
                "no id",
                vec![end_chunk(0, "", recorded())],
                vec![final_call("", None)],
                "a recorded call has no id",
            ),
        ];
        for (case, chunks, calls, reason) in cases {
            let mapper = mapper_after(chunks);
            let mut message = final_with(calls);
            let error = mapper.reconcile_recorded(&mut message).expect_err(case);
            assert_eq!(error, reason, "{case}");
        }
    }

    /// The bytes a scripted streaming plugin emits and returns.
    #[derive(Default)]
    struct ScriptedStream {
        chunks: Vec<serde_json::Value>,
        out: String,
        rc: i32,
    }

    extern "C" fn scripted_stream(
        _req: StbStringRef,
        emit: Option<rpi_plugin_sdk::ProviderStreamEmitFn>,
        emit_user_data: *mut c_void,
        _control: *const rpi_plugin_sdk::ProviderStreamControl,
        _completion: Option<rpi_plugin_sdk::ProviderStreamCompletionFn>,
        _completion_user_data: *mut c_void,
        out: *mut StbString,
        user_data: *mut c_void,
    ) -> i32 {
        let script = unsafe { &*(user_data as *const ScriptedStream) };
        if let Some(emit) = emit {
            for chunk in &script.chunks {
                if emit(StbString::from_string(chunk.to_string()), emit_user_data) != 0 {
                    return 1;
                }
            }
        }
        unsafe { *out = StbString::from_string(script.out.clone()) };
        script.rc
    }

    /// Drive `script` through the real streaming provider and return the
    /// terminal message, which must be an `Error`.
    async fn error_terminal(script: ScriptedStream) -> AssistantMessage {
        let script = Box::new(script);
        let record = RegisteredProvider {
            provider_id: "pluggy".to_string(),
            base_url: "https://example".to_string(),
            api_style: "faux".to_string(),
            request_fn: ok_request_fn,
            stream_fn: Some(scripted_stream),
            plugin_free_string: stub_free,
            user_data: &*script as *const ScriptedStream as *mut c_void,
        };
        let mut registry = ExtensionRegistry::new();
        registry.register_streaming_provider(record.clone());
        let provider = PluggableProvider::new(
            record,
            Arc::new(registry.snapshot()),
            PluginKeepalive::empty(),
            Handle::current(),
        );
        let model = Model::new(
            "m",
            "M",
            rpi_ai::types::Api::Faux,
            "pluggy",
            "https://example",
        );
        let mut stream = provider
            .stream_simple(
                &model,
                &Context::new(Vec::new()),
                &SimpleStreamOptions::default(),
            )
            .await;
        let mut last = None;
        while let Some(event) = stream.next().await {
            last = Some(event);
        }
        match last {
            Some(AssistantMessageEvent::Error { reason, error }) => {
                assert_eq!(reason, ErrorReason::Error);
                error
            }
            other => panic!("expected an Error terminal, got {other:?}"),
        }
    }

    fn recorded_stream() -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({"type":"toolcall_start","contentIndex":0,"id":"c1","name":"shell"}),
            end_chunk(0, "c1", Some(recording("upstream"))),
        ]
    }

    /// Through the real streaming driver, a final `out` that contradicts a
    /// streamed recording ends the turn with an error, so the agent loop never
    /// sees an executable copy of the call.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_contradicting_final_message_fails_the_turn() {
        let mut out = final_with(vec![final_call("c2", None)]);
        out.usage.input = 11;
        out.usage.output = 7;
        let error = error_terminal(ScriptedStream {
            chunks: recorded_stream(),
            out: serde_json::to_string(&out).unwrap(),
            ..Default::default()
        })
        .await;
        assert!(error.content.is_empty(), "{error:?}");
        assert_eq!(
            error.error_message.as_deref(),
            Some("plugin provider final message drops a recorded tool call: the call is missing")
        );
        // The plugin's usage for the turn is still reported.
        assert_eq!(error.usage, out.usage);
    }

    /// A plugin that fails after streaming a recorded call keeps its own error:
    /// it already executes nothing, so reconciliation does not replace it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_request_keeps_its_own_error_after_a_recorded_call() {
        let error = error_terminal(ScriptedStream {
            chunks: recorded_stream(),
            rc: 7,
            ..Default::default()
        })
        .await;
        let text = error.error_message.unwrap_or_default();
        assert!(text.contains("returned error code 7"), "{text}");
    }

    /// The request envelopes a capturing provider has seen.
    type SeenRequests = Mutex<Vec<serde_json::Value>>;

    fn seen(req: StbStringRef, user_data: *mut c_void) {
        let seen = unsafe { &*(user_data as *const SeenRequests) };
        let request = serde_json::from_str(unsafe { req.as_str() }).expect("request JSON");
        seen.lock().unwrap().push(request);
    }

    extern "C" fn capturing_request(
        req: StbStringRef,
        out: *mut StbString,
        user_data: *mut c_void,
    ) -> i32 {
        seen(req, user_data);
        let message = AssistantMessage::empty(rpi_ai::types::Api::Faux, "pluggy", "m", 0);
        unsafe { *out = StbString::from_string(serde_json::to_string(&message).unwrap()) };
        0
    }

    extern "C" fn capturing_stream(
        req: StbStringRef,
        _emit: Option<rpi_plugin_sdk::ProviderStreamEmitFn>,
        _emit_user_data: *mut c_void,
        _control: *const rpi_plugin_sdk::ProviderStreamControl,
        _completion: Option<rpi_plugin_sdk::ProviderStreamCompletionFn>,
        _completion_user_data: *mut c_void,
        out: *mut StbString,
        user_data: *mut c_void,
    ) -> i32 {
        capturing_request(req, out, user_data)
    }

    /// A plugin may only send `recorded` to a host that honors it: an older
    /// host ignores the key and runs the call. Both request paths therefore
    /// advertise the recording version this host decodes, and a plugin that
    /// finds no such capability must not send `recorded`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn both_request_paths_advertise_recorded_tool_calls() {
        let requests = Box::new(SeenRequests::default());
        for stream_fn in [
            None,
            Some(capturing_stream as rpi_plugin_sdk::ProviderStreamRequestFn),
        ] {
            let record = RegisteredProvider {
                provider_id: "pluggy".to_string(),
                base_url: "https://example".to_string(),
                api_style: "faux".to_string(),
                request_fn: capturing_request,
                stream_fn,
                plugin_free_string: stub_free,
                user_data: &*requests as *const SeenRequests as *mut c_void,
            };
            let mut registry = ExtensionRegistry::new();
            registry.register_provider(record.clone());
            let provider = PluggableProvider::new(
                record,
                Arc::new(registry.snapshot()),
                PluginKeepalive::empty(),
                Handle::current(),
            );
            let model = Model::new(
                "m",
                "M",
                rpi_ai::types::Api::Faux,
                "pluggy",
                "https://example",
            );
            let mut stream = provider
                .stream_simple(
                    &model,
                    &Context::new(Vec::new()),
                    &SimpleStreamOptions::default(),
                )
                .await;
            while stream.next().await.is_some() {}
        }
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert_eq!(
                request["capabilities"],
                serde_json::json!({ "recordedToolCalls": 1 }),
                "{request}"
            );
        }
    }
}
