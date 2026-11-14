//! `AgentEvent → StablePluginEvent` translation, plus the [`ExtensionEmitter`]
//! that impls [`AgentEmitter`] by subscribing to the host's
//! `broadcast::Sender<AgentEvent>`, folding each event into a stable event, and
//! fan-out dispatching to every registered handler for the event's tag — all
//! dispatch wrapped in `catch_unwind` (a panicky plugin handler must not unwind
//! across FFI).
//!
//! The 10 stable lifecycle `AgentEvent` variants fold into their matching
//! `StablePluginEvent` tags. Internal host events such as retry scheduling are
//! intentionally not projected onto the fixed 33-tag plugin ABI.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use futures::future::BoxFuture;
use rpi_agent::events::{AgentEmitter, AgentEvent};
use rpi_plugin_sdk::{EventTag, StablePluginEvent, StbString};

use crate::host_free_string;
use crate::registry::RegistrySnapshot;

#[cfg(test)]
mod lifetime_tests;

#[cfg(test)]
thread_local! {
    static TRANSLATED_EVENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FREED_EVENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Map a native [`AgentEvent`] to its pi `on()` [`EventTag`], or `None` if the
/// host event has no stable-event counterpart. This is the **fold** described
/// in the crate docs: the ten stable lifecycle variants map onto matching tags;
/// internal retry scheduling stays host-only, and the rest of the 33-category
/// surface is driven by direct host emission in B3+.
pub fn event_tag_for(event: &AgentEvent) -> Option<EventTag> {
    match event {
        AgentEvent::AgentStart => Some(EventTag::AgentStart),
        AgentEvent::AgentEnd { .. } => Some(EventTag::AgentEnd),
        AgentEvent::RetryScheduled { .. } => None,
        AgentEvent::TurnStart => Some(EventTag::TurnStart),
        // The lifecycle tag stays payload-free; the payload rides under its own
        // tag so a host that predates the payload can never be made to read
        // uninitialized union storage. See `EventTag::TurnEndPayload`.
        AgentEvent::TurnEnd { .. } => Some(EventTag::TurnEnd),
        AgentEvent::MessageStart { .. } => Some(EventTag::MessageStart),
        AgentEvent::MessageUpdate { .. } => Some(EventTag::MessageUpdate),
        AgentEvent::MessageEnd { .. } => Some(EventTag::MessageEnd),
        AgentEvent::ToolExecutionStart { .. } => Some(EventTag::ToolExecutionStart),
        AgentEvent::ToolExecutionUpdate { .. } => Some(EventTag::ToolExecutionUpdate),
        AgentEvent::ToolExecutionEnd { .. } => Some(EventTag::ToolExecutionEnd),
    }
}

/// Serialize an [`AgentMessage`](rpi_agent::message::AgentMessage) to an owning
/// `StbString` for a message-payload event. Handlers borrow its bytes;
/// the dispatcher's cleanup guard frees it after fan-out.
fn message_to_stb(message: &rpi_agent::message::AgentMessage) -> StbString {
    // AgentMessage is Serialize (#[serde(tag="kind")]); use serde_json round-trip.
    let text = serde_json::to_string(message).unwrap_or_else(|_| "null".to_string());
    StbString::from_string(text)
}

/// Normalize an assistant `StopReason` into the value a plugin branches on.
///
/// `stop`, `toolUse` and `length` are all *successful* terminations of a turn:
/// the run is going to continue (or ended cleanly) either way. `aborted` is the
/// user cancelling. Everything else is a failure.
fn turn_outcome(stop_reason: rpi_ai::types::StopReason) -> &'static str {
    use rpi_ai::types::StopReason;
    match stop_reason {
        StopReason::Stop | StopReason::ToolUse | StopReason::Length => "stop",
        StopReason::Aborted => "aborted",
        _ => "error",
    }
}

/// Payload for `turn_end`.
///
/// Carries enough for a plugin to tell a normal stop from a failure or a user
/// abort without inferring it from the last `message_end`, plus
/// `pendingMessages` — the queue depth **after** this turn settled.
fn turn_end_payload(message: &rpi_agent::message::AgentMessage, tool_results: usize) -> String {
    let stop_reason = message
        .as_assistant()
        .map(|assistant| assistant.stop_reason);
    let outcome = stop_reason.map(turn_outcome).unwrap_or("error");
    serde_json::json!({
        "outcome": outcome,
        "stopReason": stop_reason.map(|reason| reason.as_str()),
        "toolResultCount": tool_results,
    })
    .to_string()
}

/// Translate one agent event into **every** stable event it must produce.
///
/// Almost every event is one-to-one, but `turn_end` is not: existing subscribers
/// registered for the lifecycle `turn_end` tag must keep receiving an event, and
/// the payload needs a different tag so a host that predates the payload can
/// never be made to read uninitialized union storage. Emitting both keeps
/// legacy handlers working while the payload stays behind the new tag.
/// Production dispatch additionally filters each subscription before allocating.
#[cfg(test)]
fn translate_all(event: &AgentEvent) -> Vec<StablePluginEvent> {
    match event {
        AgentEvent::TurnEnd { .. } => {
            let mut out = vec![StablePluginEvent::empty(EventTag::TurnEnd)];
            if let Some(payload) = translate(event) {
                out.push(payload);
            }
            out
        }
        _ => translate(event).into_iter().collect(),
    }
}

/// Build the stable event for an `AgentEvent`, or `None` if the event carries
/// no payload that maps onto a stable variant. The caller owns the returned
/// strings; dispatch borrows them to callbacks and frees them exactly once.
pub fn translate(event: &AgentEvent) -> Option<StablePluginEvent> {
    #[cfg(test)]
    TRANSLATED_EVENTS.with(|count| count.set(count.get() + 1));
    let tag = event_tag_for(event)?;
    match event {
        AgentEvent::MessageStart { message, .. } | AgentEvent::MessageEnd { message, .. } => {
            let stb = message_to_stb(message);
            Some(StablePluginEvent::message(tag, stb))
        }
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => {
            // Match native Pi's wire contract: message_update carries the
            // provider event, not a growing message snapshot. Consumers that
            // need live text read assistantMessageEvent.delta directly.
            let payload = assistant_message_event_to_json(assistant_message_event);
            Some(StablePluginEvent::message(
                tag,
                StbString::from_string(payload),
            ))
        }

        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        }
        | AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            args,
            ..
        } => Some(StablePluginEvent::tool_call(
            tag,
            StbString::from_string(tool_call_id.clone()),
            StbString::from_string(tool_name.clone()),
            StbString::from_string(serde_json::to_string(args).unwrap_or_else(|_| "null".into())),
        )),

        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => {
            // Serialize the AgentToolResult to JSON for the result payload.
            let result_json = agent_tool_result_to_json(result);
            Some(StablePluginEvent::tool_result(
                tag,
                StbString::from_string(tool_call_id.clone()),
                StbString::from_string(tool_name.clone()),
                StbString::from_string(result_json),
                *is_error,
            ))
        }

        // `turn_end` carries a payload so a plugin can tell a normal stop from
        // a failure or a user abort without inferring it from the last
        // `message_end`, and so a remote UI can show "N queued".
        //
        // `pendingMessages` is the queue depth **after** the turn settled. The
        // drain hooks cannot answer this (they consume the queue), which is why
        // the agent loop reports it through a read-only `pending_message_count`
        // hook instead.
        AgentEvent::TurnEnd {
            message,
            tool_results,
            pending_messages,
        } => {
            let mut payload: serde_json::Value =
                serde_json::from_str(&turn_end_payload(message, tool_results.len()))
                    .unwrap_or_else(|_| serde_json::json!({}));
            // Additive: consumers that only know the older (empty) shape ignore it.
            payload["pendingMessages"] = serde_json::json!(pending_messages);
            Some(StablePluginEvent::data(
                EventTag::TurnEndPayload,
                StbString::from_string(payload.to_string()),
            ))
        }

        // No-payload events.
        AgentEvent::AgentStart | AgentEvent::TurnStart | AgentEvent::AgentEnd { .. } => {
            Some(StablePluginEvent::empty(tag))
        }

        AgentEvent::RetryScheduled { .. } => None,
    }
}

/// Serialize the stream event inside native Pi's `message_update` envelope.
///
/// Do not include `partial`: it is a cumulative snapshot and the native JSON /
/// RPC contract deliberately omits it so consumers process each delta exactly
/// once and stream size stays linear.
fn assistant_message_event_to_json(event: &rpi_ai::types::AssistantMessageEvent) -> String {
    use rpi_ai::types::AssistantMessageEvent as E;

    let value = match event {
        E::TextStart { content_index, .. } => serde_json::json!({
            "type": "text_start",
            "contentIndex": content_index,
        }),
        E::TextDelta {
            content_index,
            delta,
            ..
        } => serde_json::json!({
            "type": "text_delta",
            "contentIndex": content_index,
            "delta": delta,
        }),
        E::TextEnd {
            content_index,
            content,
            ..
        } => serde_json::json!({
            "type": "text_end",
            "contentIndex": content_index,
            "content": content,
        }),
        E::ThinkingStart { content_index, .. } => serde_json::json!({
            "type": "thinking_start",
            "contentIndex": content_index,
        }),
        E::ThinkingDelta {
            content_index,
            delta,
            ..
        } => serde_json::json!({
            "type": "thinking_delta",
            "contentIndex": content_index,
            "delta": delta,
        }),
        E::ThinkingEnd {
            content_index,
            content,
            ..
        } => serde_json::json!({
            "type": "thinking_end",
            "contentIndex": content_index,
            "content": content,
        }),
        E::ToolCallStart { content_index, .. } => serde_json::json!({
            "type": "toolcall_start",
            "contentIndex": content_index,
        }),
        E::ToolCallDelta {
            content_index,
            delta,
            ..
        } => serde_json::json!({
            "type": "toolcall_delta",
            "contentIndex": content_index,
            "delta": delta,
        }),
        E::ToolCallEnd {
            content_index,
            tool_call,
            ..
        } => serde_json::json!({
            "type": "toolcall_end",
            "contentIndex": content_index,
            "toolCall": tool_call,
        }),
        E::Start { .. } | E::Done { .. } | E::Error { .. } => {
            serde_json::json!({ "type": event_type_name(event) })
        }
    };

    serde_json::json!({ "assistantMessageEvent": value }).to_string()
}

fn event_type_name(event: &rpi_ai::types::AssistantMessageEvent) -> &'static str {
    use rpi_ai::types::AssistantMessageEvent as E;
    match event {
        E::Start { .. } => "start",
        E::TextStart { .. } => "text_start",
        E::TextDelta { .. } => "text_delta",
        E::TextEnd { .. } => "text_end",
        E::ThinkingStart { .. } => "thinking_start",
        E::ThinkingDelta { .. } => "thinking_delta",
        E::ThinkingEnd { .. } => "thinking_end",
        E::ToolCallStart { .. } => "toolcall_start",
        E::ToolCallDelta { .. } => "toolcall_delta",
        E::ToolCallEnd { .. } => "toolcall_end",
        E::Done { .. } => "done",
        E::Error { .. } => "error",
    }
}

/// Serialize an `AgentToolResult` to the JSON shape handlers expect (same shape
/// the tool adapter uses). Kept here (not reusing tool.rs's `result_to_stb`)
/// because the emitter must not depend on adapter internals, and this is the
/// event-side direction.
fn agent_tool_result_to_json(result: &rpi_agent::types::AgentToolResult) -> String {
    let mut txt = String::new();
    txt.push('{');
    txt.push_str("\"content\":[");
    for (i, c) in result.content.iter().enumerate() {
        if i > 0 {
            txt.push(',');
        }
        match c {
            rpi_agent::types::TextContentOrImage::Text(t) => {
                txt.push_str(
                    &serde_json::to_string(&serde_json::json!({ "type": "text", "text": t.text }))
                        .unwrap_or_else(|_| "\"\"".into()),
                );
            }
            rpi_agent::types::TextContentOrImage::Image(img) => {
                txt.push_str(
                    &serde_json::to_string(&serde_json::json!({
                        "type": "image",
                        "data": img.data,
                        "mimeType": img.mime_type,
                    }))
                    .unwrap_or_else(|_| "\"\"".into()),
                );
            }
        }
    }
    txt.push(']');
    txt.push_str(",\"details\":");
    txt.push_str(&serde_json::to_string(&result.details).unwrap_or_else(|_| "null".into()));
    txt.push_str(",\"terminate\":");
    txt.push_str(if result.terminate { "true" } else { "false" });
    txt.push_str(",\"addedToolNames\":");
    txt.push_str(&serde_json::to_string(&result.added_tool_names).unwrap_or_else(|_| "[]".into()));
    txt.push('}');
    txt
}

// ===========================================================================
// ExtensionEmitter — AgentEmitter impl fanning out to plugin handlers
// ===========================================================================

/// An [`AgentEmitter`] that fans each [`AgentEvent`] out to every plugin handler
/// registered for the event's tag. Built from a [`RegistrySnapshot`] (so it
/// shares the registry's staleness flag) and installed into
/// `AgentHarnessOptions.agent_emitter` **alongside** the host's
/// [`BroadcastEmitter`] — events flow to BOTH the TUI (which drains the
/// broadcast receiver) and the plugin handlers (which receive translated
/// `StablePluginEvent`s). The host composes the two via
/// [`TeeEmitter`](super::TeeEmitter); this emitter alone only dispatches to
/// plugins.
///
/// Dispatch is `catch_unwind`-wrapped: a panicking plugin handler is logged and
/// skipped (abort-on-unwind policy would be too aggressive for event dispatch
/// where one bad handler shouldn't kill the session; we log + continue, unlike
/// tool-partial dispatch which aborts because it cannot unwind across FFI).
/// Awaited observers run on blocking workers: a synchronous plugin callback may
/// wait for a host action that needs another tool future in the same batch.
pub struct ExtensionEmitter {
    snapshot: Arc<RegistrySnapshot>,
    /// Keeps the loaded cdylibs mapped for as long as this emitter (installed
    /// into `AgentHarnessOptions.agent_emitter`) may dispatch to handler fn
    /// pointers that live inside them. Cloned from the load session; the
    /// libraries unload only when every holder (adapter + emitter) drops.
    keepalive: Arc<crate::PluginKeepalive>,
}

impl ExtensionEmitter {
    /// Build an emitter over a snapshot. The `keepalive` keeps the cdylibs that
    /// own the snapshot's handler fn pointers mapped for the emitter's lifetime.
    pub fn new(snapshot: Arc<RegistrySnapshot>, keepalive: Arc<crate::PluginKeepalive>) -> Self {
        Self {
            snapshot,
            keepalive,
        }
    }

    /// Dispatch one stable event to all handlers for its tag. Each handler call
    /// is `catch_unwind`-wrapped; errors (nonzero return) are logged but do not
    /// abort the fan-out (one handler's failure doesn't block the others — mir-
    /// rors pi's per-handler try/catch).
    fn dispatch_agent(&self, event: &AgentEvent) {
        if !crate::registry::assert_active(self.snapshot.active_flag()) {
            return;
        }
        let Some(tag) = event_tag_for(event) else {
            return;
        };
        // TurnEnd has two independent subscriptions. Preserve the payload-free
        // lifecycle event even when nobody subscribes to the new payload tag,
        // and avoid allocating payload strings for empty/stale registries.
        if matches!(event, AgentEvent::TurnEnd { .. }) {
            if !self.snapshot.handlers_for(EventTag::TurnEnd).is_empty() {
                self.dispatch(&StablePluginEvent::empty(EventTag::TurnEnd));
            }
            if self
                .snapshot
                .handlers_for(EventTag::TurnEndPayload)
                .is_empty()
            {
                return;
            }
        } else if self.snapshot.handlers_for(tag).is_empty() {
            return;
        }
        if let Some(stable) = translate(event) {
            self.dispatch(&stable);
        }
    }

    fn dispatch(&self, event: &StablePluginEvent) {
        dispatch_to_handlers(&self.snapshot, event);
    }
}

/// Fan an already-built event out to the tag's handlers (or free it when no
/// handler subscribes). Shared by [`ExtensionEmitter`] and the provider-hook
/// dispatcher ([`crate::provider_hooks`]). Handlers MUST NOT free the event
/// strings; the host frees exactly once after the fan-out.
pub fn dispatch_to_handlers(snapshot: &RegistrySnapshot, event: &StablePluginEvent) {
    // Install cleanup before every early return, including stale snapshots.
    struct Cleanup<'a>(&'a StablePluginEvent);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            free_dispatched_event(self.0);
        }
    }
    let _cleanup = Cleanup(event);
    // Staleness guard: a stale registry (swapped-out session) does nothing.
    if !crate::registry::assert_active(snapshot.active_flag()) {
        return;
    }
    let handlers = snapshot.handlers_for(event.tag);
    if handlers.is_empty() {
        return;
    }
    for h in handlers {
        // P2: skip a handler whose plugin declared platforms excluding this host.
        if !crate::registry::platform_allows(&h.platforms) {
            crate::event_log::log_handler_invocation(
                event.tag,
                &h.plugin,
                "PlatformSkip",
                0,
                Some(format!("host={}", crate::registry::current_platform())),
            );
            continue;
        }
        // SAFETY: the plugin warrants `handler` + `user_data` are safe to
        // call from this thread. catch_unwind so a panic cannot cross FFI.
        let started = std::time::Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(|| (h.handler)(*event, h.user_data)));
        let duration_ms = started.elapsed().as_millis() as u64;
        match outcome {
            // `CLAIMED` from an ordinary observe dispatch is not an error — the
            // handler simply consumed the event. Only routing dispatchers act on
            // it (`dispatch_data_event_claiming`).
            Ok(rc) if rc == rpi_plugin_sdk::EVENT_HANDLER_CLAIMED => {
                crate::event_log::log_handler_invocation(
                    event.tag,
                    &h.plugin,
                    "Claimed",
                    duration_ms,
                    None,
                );
            }
            Ok(rc) if rc != 0 => {
                tracing::warn!(tag = ?event.tag, rc, "extension event handler returned nonzero");
                crate::event_log::log_handler_invocation(
                    event.tag,
                    &h.plugin,
                    "Error",
                    duration_ms,
                    Some(format!("rc={rc}")),
                );
            }
            Ok(_) => {
                crate::event_log::log_handler_invocation(
                    event.tag,
                    &h.plugin,
                    "Continue",
                    duration_ms,
                    None,
                );
            }
            Err(_) => {
                tracing::error!(tag = ?event.tag, "extension event handler panicked — skipped");
                crate::event_log::log_handler_invocation(
                    event.tag,
                    &h.plugin,
                    "Panic",
                    duration_ms,
                    None,
                );
            }
        }
    }
}

/// Build + dispatch a generic `data` event (JSON payload) to the tag's
/// handlers. Returns whether any handler was invoked. Used by the B4
/// provider-hook observer path ([`crate::provider_hooks`]) which has no
/// matching `AgentEvent` to translate.
pub fn dispatch_data_event(snapshot: &RegistrySnapshot, tag: EventTag, data: &str) -> bool {
    let handlers = snapshot.handlers_for(tag);
    if handlers.is_empty() {
        return false;
    }
    let event = StablePluginEvent::data(tag, StbString::from_string(data.to_string()));
    dispatch_to_handlers(snapshot, &event);
    true
}

/// Build + dispatch a **no-payload** event (e.g. `session_start`, `session_shutdown`,
/// the rpi-specific `before_tui_start`) to the tag's handlers. Returns whether any
/// handler was invoked. Mirrors [`dispatch_data_event`] for events whose payload
/// is `EventEmpty`; a handler that subscribes to a no-payload tag receives the
/// event with an empty payload.
pub fn dispatch_empty_event(snapshot: &RegistrySnapshot, tag: EventTag) -> bool {
    let handlers = snapshot.handlers_for(tag);
    if handlers.is_empty() {
        return false;
    }
    let event = StablePluginEvent::empty(tag);
    dispatch_to_handlers(snapshot, &event);
    true
}

/// Timeout budget for a `ToolCall` approval dispatch. Generous because the
/// handler may wait on a human (e.g. an ACP `session/request_permission`).
pub const TOOL_CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Dispatch a tool approval against a live session. Only a successful continue
/// permits execution; stale sessions, cancellation, timeout and handler errors
/// block it. Blocking callbacks retain the plugin libraries until they return,
/// even when the async caller has already stopped waiting.
pub async fn dispatch_tool_call_event(
    session: &crate::ExtensionSession,
    tool_call_id: &str,
    tool_name: &str,
    params: &serde_json::Value,
    signal: &tokio_util::sync::CancellationToken,
    ui_dialog: &crate::UiDialogMailbox,
) -> Option<String> {
    dispatch_tool_call_event_with_timeout(
        session,
        tool_call_id,
        tool_name,
        params,
        signal,
        ui_dialog,
        TOOL_CALL_TIMEOUT,
    )
    .await
}

async fn dispatch_tool_call_event_with_timeout(
    session: &crate::ExtensionSession,
    tool_call_id: &str,
    tool_name: &str,
    params: &serde_json::Value,
    signal: &tokio_util::sync::CancellationToken,
    ui_dialog: &crate::UiDialogMailbox,
    timeout: std::time::Duration,
) -> Option<String> {
    let snapshot = session.snapshot()?;
    if !crate::registry::assert_active(snapshot.active_flag()) {
        return Some(format!(
            "tool `{tool_name}` approval session is stale; retry after reload"
        ));
    }
    for h in snapshot.handlers_for(EventTag::ToolCall).iter().cloned() {
        if !crate::registry::platform_allows(&h.platforms) {
            continue;
        }
        if signal.is_cancelled() {
            ui_dialog.cancel_tool_call(tool_call_id);
            return Some(format!("tool `{tool_name}` approval cancelled"));
        }
        if !crate::registry::assert_active(snapshot.active_flag()) {
            return Some(format!(
                "tool `{tool_name}` approval session is stale; retry after reload"
            ));
        }
        let plugin_name = h.plugin.clone();
        let id = tool_call_id.to_string();
        let name = tool_name.to_string();
        let payload = params.to_string();
        let keepalive = session.keepalive();
        let started = std::time::Instant::now();
        // Capture the whole RegisteredHandler rather than its raw user_data.
        // The callback's own keepalive also survives timeout/cancellation.
        let background = rpi_agent::tool_execution_scope::track_background_tool();
        let mut task = tokio::task::spawn_blocking(move || {
            let _keepalive = keepalive;
            let _background = background;
            call_tool_call_handler(h, id, name, payload)
        });
        let outcome = tokio::select! {
            biased;
            _ = signal.cancelled() => {
                task.abort(); // Prevent a queued callback from starting.
                ui_dialog.cancel_tool_call(tool_call_id);
                crate::event_log::log_handler_invocation(
                    EventTag::ToolCall, &plugin_name, "Cancelled",
                    started.elapsed().as_millis() as u64, None,
                );
                return Some(format!("tool `{tool_name}` approval cancelled"));
            }
            outcome = tokio::time::timeout(timeout, &mut task) => outcome,
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        let (status, error) = match outcome {
            Ok(Ok(Ok(rc))) if rc == rpi_plugin_sdk::EVENT_HANDLER_CONTINUE => ("Continue", None),
            Ok(Ok(Ok(rc))) if rc == rpi_plugin_sdk::EVENT_HANDLER_ABORT => (
                "Abort",
                Some(format!(
                    "extension `{plugin_name}` blocked tool `{tool_name}`"
                )),
            ),
            Ok(Ok(Ok(rc))) => (
                "Error",
                Some(format!(
                    "extension `{plugin_name}` tool `{tool_name}` approval failed: rc={rc}"
                )),
            ),
            Ok(Ok(Err(_))) => (
                "Panic",
                Some(format!(
                    "extension `{plugin_name}` tool `{tool_name}` approval panicked"
                )),
            ),
            Ok(Err(error)) => (
                "JoinFailed",
                Some(format!(
                    "extension `{plugin_name}` tool `{tool_name}` approval task failed: {error}"
                )),
            ),
            Err(_) => {
                task.abort();
                (
                    "Timeout",
                    Some(format!(
                        "extension `{plugin_name}` tool `{tool_name}` approval timed out"
                    )),
                )
            }
        };
        crate::event_log::log_handler_invocation(
            EventTag::ToolCall,
            &plugin_name,
            status,
            duration_ms,
            error.clone(),
        );
        if let Some(reason) = error {
            ui_dialog.cancel_tool_call(tool_call_id);
            return Some(reason);
        }
        // A reload can invalidate this session while the callback is running.
        if !crate::registry::assert_active(snapshot.active_flag()) {
            ui_dialog.cancel_tool_call(tool_call_id);
            return Some(format!(
                "tool `{tool_name}` approval session is stale; retry after reload"
            ));
        }
    }
    None
}

/// Like [`dispatch_data_event`], but reports whether a handler **claimed** the
/// event by returning [`EVENT_HANDLER_CLAIMED`].
///
/// This exists for input routing: the TUI asks extensions "is this key yours?"
/// and must know whether to swallow the key (skip the editor) or pass it
/// through. A handler that subscribes but is currently inactive (e.g.
/// push-to-talk toggled off) returns the ordinary continue code and the key
/// falls through to normal editor handling.
///
/// Every handler is still invoked (fan-out is not short-circuited) so a later
/// handler can disagree; the event is considered claimed if **any** returned
/// [`EVENT_HANDLER_CLAIMED`].
pub fn dispatch_data_event_claiming(
    snapshot: &RegistrySnapshot,
    tag: EventTag,
    data: &str,
) -> bool {
    if !crate::registry::assert_active(snapshot.active_flag()) {
        return false;
    }
    let handlers = snapshot.handlers_for(tag);
    if handlers.is_empty() {
        return false;
    }
    let event = StablePluginEvent::data(tag, StbString::from_string(data.to_string()));
    let mut claimed = false;
    for h in handlers {
        if !crate::registry::platform_allows(&h.platforms) {
            continue;
        }
        // SAFETY: as in `dispatch_to_handlers` — the plugin warrants the handler
        // and its user_data are safe to call from this thread.
        let outcome = catch_unwind(AssertUnwindSafe(|| (h.handler)(event, h.user_data)));
        match outcome {
            Ok(rc) if rc == rpi_plugin_sdk::EVENT_HANDLER_CLAIMED => claimed = true,
            Ok(rc) if rc != 0 => {
                tracing::warn!(tag = ?tag, rc, "extension event handler returned nonzero");
            }
            Err(_) => {
                tracing::error!(tag = ?tag, "extension event handler panicked — skipped");
            }
            Ok(_) => {}
        }
    }
    free_dispatched_event(&event);
    claimed
}

/// Per-lifecycle-event handler timeout budget (see docs/extensions/lifecycle.md):
/// startup 5s, session shutdown 15s, everything else 10s. A handler that
/// exceeds its budget is logged + skipped (it keeps running on its blocking
/// thread, but no longer gates startup/shutdown).
pub fn lifecycle_timeout_for(tag: EventTag) -> std::time::Duration {
    use rpi_plugin_sdk::EventTag as T;
    match tag {
        T::BeforeTuiStart => std::time::Duration::from_secs(5),
        T::SessionShutdown => std::time::Duration::from_secs(15),
        _ => std::time::Duration::from_secs(10),
    }
}

/// Invoke one no-payload lifecycle handler, catching unwinds at the FFI boundary.
///
/// Takes [`RegisteredHandler`](crate::registry::RegisteredHandler) **by value**
/// so the caller's `spawn_blocking` closure captures the whole struct (which has
/// `unsafe impl Send`) rather than its disjoint fields: `user_data` is a raw
/// `*mut c_void` and is NOT `Send` on its own (Rust 2021 disjoint closure
/// capture would otherwise move the raw pointer directly into the closure env
/// and fail the `Send` bound).
fn call_lifecycle_handler(
    h: crate::registry::RegisteredHandler,
    tag: EventTag,
) -> std::thread::Result<i32> {
    let event = StablePluginEvent::empty(tag);
    // SAFETY: the plugin warrants `h.handler` + `h.user_data` are safe to call
    // from any thread; catch_unwind so a panic cannot cross FFI.
    catch_unwind(AssertUnwindSafe(|| (h.handler)(event, h.user_data)))
}

/// Invoke one `ToolCall` handler on a blocking thread. Takes the whole
/// [`RegisteredHandler`](crate::registry::RegisteredHandler) **by value** for the
/// same `Send` reason as [`call_lifecycle_handler`]: the closure must capture the
/// `Send` struct whole, not its non-`Send` `user_data` field. Builds and frees
/// the tool-call event on this thread (a `StablePluginEvent` holds raw pointers).
fn call_tool_call_handler(
    h: crate::registry::RegisteredHandler,
    tool_call_id: String,
    tool_name: String,
    params: String,
) -> std::thread::Result<i32> {
    let event = StablePluginEvent::tool_call(
        EventTag::ToolCall,
        StbString::from_string(tool_call_id),
        StbString::from_string(tool_name),
        StbString::from_string(params),
    );
    // SAFETY: the plugin warrants `h.handler` + `h.user_data` are safe to call
    // from any thread; catch_unwind so a panic cannot cross FFI.
    let rc = catch_unwind(AssertUnwindSafe(|| (h.handler)(event, h.user_data)));
    free_dispatched_event(&event);
    rc
}

/// Veto-aware, timeout-bounded dispatch for a **no-payload lifecycle event**
/// (`BeforeTuiStart` / `SessionStart` / `SessionShutdown`).
///
/// Unlike [`dispatch_empty_event`] (fire-and-forget observe fan-out), this:
///
/// 1. Runs each handler on a blocking thread with a per-event timeout
///    ([`lifecycle_timeout_for`]) — a hung handler is logged + skipped, it
///    does not stall startup or shutdown.
/// 2. Honors the SDK's [`EVENT_HANDLER_ABORT`] veto code: the first handler to
///    return it stops the fan-out and this returns `Some(reason)`.
///
/// Returns `None` when no handler vetoed (continue the lifecycle); `Some` when
/// a handler aborted. A stale registry (swapped-out session) returns `None`.
/// The **caller** decides what a veto means for the phase — e.g. a
/// `BeforeTuiStart` veto aborts startup before the TUI initializes.
/// `keepalive` must own this snapshot's libraries. Each blocking callback keeps
/// its own clone even if the dispatch times out or its future is dropped.
pub async fn dispatch_lifecycle_event(
    snapshot: &RegistrySnapshot,
    tag: EventTag,
    keepalive: Arc<crate::PluginKeepalive>,
) -> Option<String> {
    // Staleness guard: a stale registry (swapped-out session) does nothing.
    if !crate::registry::assert_active(snapshot.active_flag()) {
        return None;
    }
    let handlers = snapshot.handlers_for(tag).to_vec();
    if handlers.is_empty() {
        return None;
    }
    // Lifecycle tags are no-payload: the event is `EventEmpty`. We build it
    // inside the helper (via `spawn_blocking`) so the closure env only carries
    // Send values.
    let timeout = lifecycle_timeout_for(tag);
    for h in handlers {
        // P2: skip a handler whose plugin declared platforms excluding this host.
        if !crate::registry::platform_allows(&h.platforms) {
            crate::event_log::log_handler_invocation(
                tag,
                &h.plugin,
                "PlatformSkip",
                0,
                Some(format!("host={}", crate::registry::current_platform())),
            );
            continue;
        }
        // Capture the owning extension's display name for the veto message
        // before the handler is moved into the blocking closure.
        let plugin_name = h.plugin.clone();
        let h_task = h;
        let tag_task = tag;
        let started = std::time::Instant::now();
        let keepalive = keepalive.clone();
        let background = rpi_agent::tool_execution_scope::track_background_tool();
        let outcome = tokio::time::timeout(
            timeout,
            tokio::task::spawn_blocking(move || {
                let _keepalive = keepalive;
                let _background = background;
                call_lifecycle_handler(h_task, tag_task)
            }),
        )
        .await;
        let duration_ms = started.elapsed().as_millis() as u64;
        match outcome {
            Ok(Ok(Ok(rc))) if rc == rpi_plugin_sdk::EVENT_HANDLER_ABORT => {
                tracing::warn!(
                    tag = ?tag,
                    plugin = %plugin_name,
                    "extension handler vetoed lifecycle event"
                );
                crate::event_log::log_handler_invocation(
                    tag,
                    &plugin_name,
                    "Abort",
                    duration_ms,
                    None,
                );
                return Some(format!("extension `{plugin_name}` vetoed {tag:?}"));
            }
            Ok(Ok(Ok(rc))) => {
                let result = if rc == 0 { "Continue" } else { "Error" };
                crate::event_log::log_handler_invocation(
                    tag,
                    &plugin_name,
                    result,
                    duration_ms,
                    if rc == 0 {
                        None
                    } else {
                        Some(format!("rc={rc}"))
                    },
                );
            }
            Ok(Ok(Err(_))) => {
                tracing::error!(tag = ?tag, "extension handler panicked — skipped");
                crate::event_log::log_handler_invocation(
                    tag,
                    &plugin_name,
                    "Panic",
                    duration_ms,
                    None,
                );
            }
            Ok(Err(join_err)) => {
                tracing::error!(
                    tag = ?tag,
                    error = %join_err,
                    "extension handler task join failed — skipped"
                );
                crate::event_log::log_handler_invocation(
                    tag,
                    &plugin_name,
                    "JoinFailed",
                    duration_ms,
                    Some(join_err.to_string()),
                );
            }
            Err(_elapsed) => {
                tracing::warn!(
                    tag = ?tag,
                    timeout_ms = timeout.as_millis(),
                    "extension handler timed out — skipped (still running on its blocking thread)"
                );
                crate::event_log::log_handler_invocation(
                    tag,
                    &plugin_name,
                    "Timeout",
                    duration_ms,
                    Some(format!("budget_ms={}", timeout.as_millis())),
                );
            }
        }
    }
    None
}

impl AgentEmitter for ExtensionEmitter {
    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        let subscribed = match event_tag_for(&event) {
            Some(EventTag::TurnEnd) => {
                !self.snapshot.handlers_for(EventTag::TurnEnd).is_empty()
                    || !self
                        .snapshot
                        .handlers_for(EventTag::TurnEndPayload)
                        .is_empty()
            }
            Some(tag) => !self.snapshot.handlers_for(tag).is_empty(),
            None => false,
        };
        if !crate::registry::assert_active(self.snapshot.active_flag()) || !subscribed {
            return Box::pin(async {});
        }
        let snapshot = Arc::clone(&self.snapshot);
        let keepalive = Arc::clone(&self.keepalive);
        Box::pin(async move {
            // Awaiting preserves event ordering while the parallel batch keeps
            // polling its owned tool futures. block_in_place inside a callback
            // only releases a Tokio worker; it cannot poll those sibling futures.
            // The closure owns its libraries even if its caller is cancelled.
            let background = rpi_agent::tool_execution_scope::track_background_tool();
            let result = tokio::task::spawn_blocking(move || {
                let _background = background;
                let emitter = ExtensionEmitter {
                    snapshot,
                    keepalive,
                };
                #[cfg(test)]
                let before = (
                    TRANSLATED_EVENTS.with(|count| count.get()),
                    FREED_EVENTS.with(|count| count.get()),
                );
                emitter.dispatch_agent(&event);
                // Test-only accounting follows the awaited dispatch across the
                // blocking boundary without making counters global to tests.
                #[cfg(test)]
                {
                    (
                        TRANSLATED_EVENTS.with(|count| count.get()) - before.0,
                        FREED_EVENTS.with(|count| count.get()) - before.1,
                    )
                }
            })
            .await;
            #[cfg(test)]
            if let Ok((translated, freed)) = &result {
                TRANSLATED_EVENTS.with(|count| count.set(count.get() + translated));
                FREED_EVENTS.with(|count| count.set(count.get() + freed));
            }
            if let Err(error) = result {
                tracing::error!(%error, "extension observer task failed");
            }
        })
    }

    fn try_emit(&self, event: AgentEvent) {
        self.dispatch_agent(&event);
    }
}

// ===========================================================================
// TeeEmitter — fan an AgentEvent out to N AgentEmitters (host + plugins)
// ===========================================================================

/// An [`AgentEmitter`] that forwards every event to each of its children, in
/// registration order. The host builds one around `[BroadcastEmitter (→ TUI),
/// ExtensionEmitter (→ plugin handlers)]` so a single `AgentHarnessOptions
/// .agent_emitter` slot feeds both consumers: the TUI keeps rendering from its
/// broadcast receiver, and plugin `on()` handlers receive translated
/// `StablePluginEvent`s.
///
/// `emit` awaits each child in turn (the loop awaits `emit`, so order matches
/// registration); `try_emit` calls each child's `try_emit` (the tool
/// `on_update` path — non-blocking). A child that panics is isolated by the
/// child's own `catch_unwind` where applicable (the `ExtensionEmitter` does);
/// the `BroadcastEmitter` cannot panic (it's a `tx.send`). We do NOT wrap the
/// fan-out itself in `catch_unwind` — each child is responsible for its own
/// soundness, and a generic wrapper would mask a child's contract violation.
pub struct TeeEmitter {
    emitters: Vec<Arc<dyn AgentEmitter>>,
}

impl TeeEmitter {
    /// Build a tee over the given emitters. Order is preserved: `emit`/`try_emit`
    /// visit them front-to-back. A single-child tee is a trivial passthrough
    /// (the host uses that when no extensions loaded, so the code path is
    /// uniform).
    pub fn new(emitters: Vec<Arc<dyn AgentEmitter>>) -> Self {
        Self { emitters }
    }
}

impl AgentEmitter for TeeEmitter {
    fn tool_program_observer(
        &self,
    ) -> Option<Arc<dyn rpi_agent::tool_program_observation::ToolProgramObserver>> {
        let mut owner = None;
        for emitter in &self.emitters {
            if let Some(candidate) = emitter.tool_program_observer() {
                if let Some(selected) = &owner {
                    if !Arc::ptr_eq(selected, &candidate) {
                        // Observation is single-owner, unlike ordinary event
                        // fan-out. Never silently drop it or choose authority
                        // by registration order. Refuse before child effects.
                        return Some(Arc::new(AmbiguousToolProgramObservers));
                    }
                } else {
                    owner = Some(candidate);
                }
            }
        }
        owner
    }

    fn emit(&self, event: AgentEvent) -> BoxFuture<'static, ()> {
        // We can't hold `&self` across an await boundary into a 'static future
        // cheaply here without cloning the Arcs — so clone them and drive the
        // fan-out inside a pinned async block. Await each child so asynchronous
        // extension observers retain the same order as the host broadcast.
        let emitters = self.emitters.clone();
        Box::pin(async move {
            for e in &emitters {
                e.emit(event.clone()).await;
            }
        })
    }

    fn try_emit(&self, event: AgentEvent) {
        for e in &self.emitters {
            e.try_emit(event.clone());
        }
    }
}

struct AmbiguousToolProgramObservers;
impl rpi_agent::tool_program_observation::ToolProgramObserver for AmbiguousToolProgramObservers {
    fn begin<'a>(
        &'a self,
        _child: rpi_agent::tool_program_observation::ToolProgramChild,
        _arguments: &'a serde_json::Value,
        _signal: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<
        'a,
        Result<Arc<dyn rpi_agent::tool_program_observation::ToolProgramObservation>, String>,
    > {
        Box::pin(async { Err("multiple distinct ToolProgram observers are not supported".into()) })
    }
}

/// Free every owning `StbString` in a dispatched event exactly once via the
/// host's `free_string`. Called by [`ExtensionEmitter::dispatch`] after the
/// fan-out completes. Handlers MUST NOT free event strings (host owns cleanup).
fn free_dispatched_event(event: &StablePluginEvent) {
    #[cfg(test)]
    FREED_EVENTS.with(|count| count.set(count.get() + 1));
    use rpi_plugin_sdk::EventTag as T;
    match event.tag {
        T::MessageStart | T::MessageUpdate | T::MessageEnd => {
            // SAFETY: tag matches the message variant.
            unsafe { host_free_string(event.payload.message.message) };
        }
        T::ToolCall | T::ToolExecutionStart | T::ToolExecutionUpdate => {
            // SAFETY: tag matches the tool_call variant.
            unsafe {
                let tc = &event.payload.tool_call;
                host_free_string(tc.tool_call_id);
                host_free_string(tc.tool_name);
                host_free_string(tc.params);
            }
        }
        T::ToolResult | T::ToolExecutionEnd => {
            // SAFETY: tag matches the tool_result variant.
            unsafe {
                let tr = &event.payload.tool_result;
                host_free_string(tr.tool_call_id);
                host_free_string(tr.tool_name);
                host_free_string(tr.result);
            }
        }
        T::ProjectTrust
        | T::ResourcesDiscover
        | T::SessionStart
        | T::SessionInfoChanged
        | T::SessionBeforeSwitch
        | T::SessionBeforeFork
        | T::SessionBeforeCompact
        | T::SessionCompact
        | T::SessionShutdown
        | T::SessionBeforeTree
        | T::SessionTree
        | T::Context
        | T::BeforeAgentStart
        | T::AgentStart
        | T::AgentEnd
        | T::AgentSettled
        | T::TurnStart
        // Lifecycle only: the `turn_end` payload rides under
        // `TurnEndPayload`, so this tag owns no allocation.
        | T::TurnEnd
        | T::ModelSelect
        | T::ThinkingLevelSelect
        | T::UserBash
        | T::BeforeTuiStart
        | T::UiPromptStart
        | T::UiPromptEnd => {
            // no payload today.
        }
        // The B4 provider-hook observer events, and the `Input` /`EditorChange`
        // events dispatched by the TUI, carry a generic data payload (built by
        // `dispatch_data_event`); free the single StbString.
        T::BeforeProviderRequest
        | T::BeforeProviderHeaders
        | T::AfterProviderResponse
        | T::Input
        | T::EditorChange
        // `turn_end` carries `{outcome, stopReason, toolResultCount,
        // pendingMessages}` as an owning `data` allocation under its own tag.
        // Leaving it in the no-payload arm leaked one allocation per event.
        | T::TurnEndPayload => {
            // SAFETY: these tags are only ever constructed as data payloads.
            unsafe { host_free_string(event.payload.data.data) };
        }
    }
}

// SAFETY note on the `unsafe { host_free_string(...) }` calls above:
// Each arm selects the initialized union payload and transfers its live
// host-produced allocation to host_free_string exactly once. Both the union
// access and the deallocator's ownership contract require this boundary.

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_agent::message::AgentMessage;
    use rpi_agent::types::AgentToolResult;
    use rpi_ai::types::{AssistantMessage, AssistantMessageEvent, Usage};
    use rpi_plugin_sdk::EventTag;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    // The two handler-fan-out tests share the process-global `HANDLER_HITS`
    // counter (the handler is an `extern "C" fn` that can't capture per-test
    // state). Parallel #[test] execution would have one test's `store(0)`
    // wipe the other's in-flight increment. Hold this lock for the ENTIRE
    // body of both tests so their reset/dispatch windows don't overlap. The
    // dispatch is synchronous (fn-pointer calls), so the guard is released
    // before the test returns — no handler outlives the test.
    static HANDLER_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// `turn_end` must carry `pendingMessages`, and it must be the queue depth
    /// **after** the turn settled.
    ///
    /// This is the only data source for a remote UI's "N queued" indicator: the
    /// drain hooks consume the queue, so the count has to come from the
    /// A handler registered for the lifecycle `turn_end` tag must still run.
    ///
    /// Regression: routing the payload under its own tag replaced the lifecycle
    /// event instead of accompanying it, so every existing `turn_end` subscriber
    /// received nothing (observed 0 calls where 1 is expected).
    #[test]
    fn legacy_turn_end_subscribers_still_receive_the_lifecycle_event() {
        static TURN_END_CALLS: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        extern "C" fn count_turn_end(
            _ev: rpi_plugin_sdk::StablePluginEvent,
            _ud: *mut std::ffi::c_void,
        ) -> i32 {
            TURN_END_CALLS.fetch_add(1, Ordering::SeqCst);
            0
        }

        let mut reg = crate::registry::ExtensionRegistry::new();
        reg.register_event_handler(
            "legacy".to_string(),
            EventTag::TurnEnd,
            count_turn_end,
            std::ptr::null_mut(),
        );
        let snapshot = reg.snapshot();

        let event = AgentEvent::TurnEnd {
            message: rpi_agent::AgentMessage::Assistant(Box::new(
                rpi_ai::types::AssistantMessage::empty(
                    rpi_ai::types::Api::AnthropicMessages,
                    "p",
                    "m",
                    0,
                ),
            )),
            tool_results: vec![],
            pending_messages: 0,
        };
        TURN_END_CALLS.store(0, Ordering::SeqCst);
        for stable in translate_all(&event) {
            dispatch_to_handlers(&snapshot, &stable);
        }
        assert_eq!(
            TURN_END_CALLS.load(Ordering::SeqCst),
            1,
            "a legacy `turn_end` subscriber must still be called exactly once"
        );
    }

    /// The `turn_end` payload tag owns an allocation, so it must be freed on
    /// every dispatch path — including zero subscribers, where no handler runs.
    ///
    /// A leak here is invisible without accounting, so this asserts the arm the
    /// cleanup takes rather than pretending to measure the allocator.
    #[test]
    fn turn_end_payload_is_freed_on_the_zero_subscriber_path() {
        let event = StablePluginEvent::data(
            EventTag::TurnEndPayload,
            StbString::from_string(r#"{"pendingMessages":2}"#.to_string()),
        );
        // Reading it back first proves the payload is live and owning.
        let view = unsafe { rpi_plugin_sdk::events::EventView::from_raw(&event) };
        assert_eq!(
            view.data_text().as_deref(),
            Some(r#"{"pendingMessages":2}"#)
        );
        // The empty registry takes the `handlers.is_empty()` branch, which must
        // free the allocation instead of leaving it to a handler that never runs.
        let snapshot = crate::registry::ExtensionRegistry::new().snapshot();
        dispatch_to_handlers(&snapshot, &event);
    }

    /// The lifecycle `turn_end` tag owns nothing now that the payload moved to
    /// its own tag, so freeing it must be a no-op rather than a stray free.
    #[test]
    fn turn_end_lifecycle_event_owns_no_payload() {
        let event = StablePluginEvent::empty(EventTag::TurnEnd);
        let view = unsafe { rpi_plugin_sdk::events::EventView::from_raw(&event) };
        assert_eq!(view.data_text(), None);
        let snapshot = crate::registry::ExtensionRegistry::new().snapshot();
        dispatch_to_handlers(&snapshot, &event);
    }

    /// read-only `pending_message_count` hook.
    #[test]
    fn turn_end_carries_pending_message_count() {
        let assistant = AgentMessage::Assistant(Box::new(AssistantMessage {
            role: rpi_ai::types::AssistantRole,
            content: vec![rpi_ai::types::Content::text("hi")],
            api: rpi_ai::types::Api::AnthropicMessages,
            provider: "anthropic".to_string(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: rpi_ai::types::StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        }));
        let event = AgentEvent::TurnEnd {
            message: assistant,
            tool_results: vec![],
            pending_messages: 3,
        };
        let translated = translate(&event).expect("turn_end maps to a tag");
        // The payload rides under its own tag so an old host — which only sends
        // the payload-free `TurnEnd` — can never be read through the union.
        assert_eq!(translated.tag, EventTag::TurnEndPayload);
        // SAFETY: `TurnEndPayload` carries the `data` variant; discriminating by
        // tag is the documented contract for this union.
        let payload = unsafe { translated.payload.data.data.to_string_lossy() };
        assert!(
            !payload.is_empty(),
            "turn_end must carry a payload (an older host sent none)"
        );
        let parsed: serde_json::Value = serde_json::from_str(&payload).expect("valid JSON");
        assert_eq!(parsed["pendingMessages"], 3, "{payload}");
        assert_eq!(parsed["outcome"], "stop", "{payload}");
        assert_eq!(parsed["toolResultCount"], 0, "{payload}");
    }

    #[test]
    fn all_ten_agent_events_map_to_a_tag() {
        let am = AgentMessage::Assistant(Box::new(AssistantMessage {
            role: rpi_ai::types::AssistantRole,
            content: vec![rpi_ai::types::Content::text("hi")],
            api: rpi_ai::types::Api::AnthropicMessages,
            provider: "anthropic".to_string(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: rpi_ai::types::StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        }));
        let events = vec![
            AgentEvent::AgentStart,
            AgentEvent::AgentEnd { messages: vec![] },
            AgentEvent::TurnStart,
            AgentEvent::TurnEnd {
                message: am.clone(),
                tool_results: vec![],
                pending_messages: 0,
            },
            AgentEvent::MessageStart {
                message_id: None,
                message: am.clone(),
            },
            AgentEvent::MessageUpdate {
                message_id: None,
                // `MessageUpdate` carries the shared partial snapshot, so build
                // the same `Arc` the provider would have handed over.
                message: std::sync::Arc::new((*am.as_assistant().unwrap()).clone()),
                assistant_message_event: rpi_ai::types::AssistantMessageEvent::Start {
                    partial: std::sync::Arc::new((*am.as_assistant().unwrap()).clone()),
                },
            },
            AgentEvent::MessageEnd {
                message_id: None,
                message: am,
            },
            AgentEvent::ToolExecutionStart {
                tool_call_id: "c1".into(),
                tool_name: "echo".into(),
                args: serde_json::json!({}),
            },
            AgentEvent::ToolExecutionUpdate {
                tool_call_id: "c1".into(),
                tool_name: "echo".into(),
                args: serde_json::json!({}),
                partial_result: std::sync::Arc::new(AgentToolResult::text("...")),
            },
            AgentEvent::ToolExecutionEnd {
                tool_call_id: "c1".into(),
                tool_name: "echo".into(),
                result: AgentToolResult::text("done"),
                is_error: false,
            },
        ];
        for e in &events {
            assert!(
                event_tag_for(e).is_some(),
                "event {:?} should map",
                e.type_tag()
            );
        }
        // Spot-check the fold targets.
        assert_eq!(event_tag_for(&events[0]), Some(EventTag::AgentStart));
        assert_eq!(event_tag_for(&events[2]), Some(EventTag::TurnStart));
        assert_eq!(
            event_tag_for(&events[7]),
            Some(EventTag::ToolExecutionStart)
        );
        assert!(event_tag_for(&AgentEvent::RetryScheduled {
            attempt: 1,
            max_retries: 10,
            delay_ms: 2_000,
            error: "503".into(),
        })
        .is_none());
    }

    #[test]
    fn translate_message_update_forwards_native_text_deltas_without_snapshot() {
        let partial = Arc::new(AssistantMessage {
            role: rpi_ai::types::AssistantRole,
            content: vec![rpi_ai::types::Content::text("你好世界")],
            api: rpi_ai::types::Api::AnthropicMessages,
            provider: "anthropic".to_string(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: rpi_ai::types::StopReason::Pending,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        });
        let translate_delta = |delta: &str| {
            let event = AgentEvent::MessageUpdate {
                message_id: None,
                message: Arc::clone(&partial),
                assistant_message_event: AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: delta.to_string(),
                    partial: Arc::clone(&partial),
                },
            };
            let stable = translate(&event).expect("message_update maps");
            let payload = unsafe { stable.payload.message.message.to_string_lossy() };
            unsafe { host_free_string(stable.payload.message.message) };
            serde_json::from_str::<serde_json::Value>(&payload).expect("valid payload")
        };

        let first = translate_delta("你好");
        let second = translate_delta("世界");
        assert_eq!(first["assistantMessageEvent"]["type"], "text_delta");
        assert_eq!(first["assistantMessageEvent"]["contentIndex"], 0);
        assert_eq!(first["assistantMessageEvent"]["delta"], "你好");
        assert_eq!(second["assistantMessageEvent"]["delta"], "世界");
        assert!(first.get("message").is_none());
        assert!(first["assistantMessageEvent"].get("partial").is_none());
    }

    #[test]
    fn translate_message_end_produces_message_payload() {
        let am = AgentMessage::Assistant(Box::new(AssistantMessage {
            role: rpi_ai::types::AssistantRole,
            content: vec![rpi_ai::types::Content::text("hi")],
            api: rpi_ai::types::Api::AnthropicMessages,
            provider: "anthropic".to_string(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: rpi_ai::types::StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        }));
        let ev = AgentEvent::MessageEnd {
            message_id: None,
            message: am,
        };
        let stable = translate(&ev).expect("maps");
        assert_eq!(stable.tag, EventTag::MessageEnd);
        // free the payload string (host owns cleanup).
        // SAFETY: tag == MessageEnd.
        unsafe { host_free_string(stable.payload.message.message) };
    }

    // --- an emitter fan-out test with an in-process handler -----------------

    static HANDLER_HITS: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn counting_handler(_ev: StablePluginEvent, _ud: *mut std::ffi::c_void) -> i32 {
        HANDLER_HITS.fetch_add(1, Ordering::SeqCst);
        0
    }

    struct WaitingObserver {
        released: Mutex<bool>,
        ready: std::sync::Condvar,
        entered: tokio::sync::Notify,
        completed: std::sync::atomic::AtomicBool,
    }

    extern "C" fn waiting_observer(_ev: StablePluginEvent, data: *mut std::ffi::c_void) -> i32 {
        // The test owns this allocation until the awaited callback returns.
        let state = unsafe { &*data.cast::<WaitingObserver>() };
        state.entered.notify_one();
        let (released, _) = state
            .ready
            .wait_timeout_while(
                state.released.lock().unwrap(),
                std::time::Duration::from_secs(5),
                |released| !*released,
            )
            .unwrap();
        state.completed.store(*released, Ordering::SeqCst);
        0
    }

    #[tokio::test]
    async fn awaited_observer_allows_sibling_futures_to_progress() {
        let state = Box::new(WaitingObserver {
            released: Mutex::new(false),
            ready: std::sync::Condvar::new(),
            entered: tokio::sync::Notify::new(),
            completed: std::sync::atomic::AtomicBool::new(false),
        });
        let mut registry = crate::registry::ExtensionRegistry::new();
        registry.register_event_handler(
            "waiting-observer".into(),
            EventTag::ToolExecutionStart,
            waiting_observer,
            (&*state as *const WaitingObserver).cast_mut().cast(),
        );
        let emitter = ExtensionEmitter::new(
            Arc::new(registry.snapshot()),
            crate::loader::PluginKeepalive::empty(),
        );
        // Model a plugin waiting for a host action that needs a sibling tool
        // future owned by this same parent task. A synchronous emit blocks
        // before join can ever poll that sibling; the finite callback timeout
        // makes that regression fail instead of hanging the test process.
        let dispatch = emitter.emit(AgentEvent::ToolExecutionStart {
            tool_call_id: "call".into(),
            tool_name: "read".into(),
            args: serde_json::json!({}),
        });
        tokio::join!(dispatch, async {
            *state.released.lock().unwrap() = true;
            state.ready.notify_one();
        });
        assert!(
            state.completed.load(Ordering::SeqCst),
            "observer blocked the sibling future it depended on"
        );
    }

    #[tokio::test]
    async fn cancelled_observer_retains_background_scope_until_callback_finishes() {
        let state = Box::new(WaitingObserver {
            released: Mutex::new(false),
            ready: std::sync::Condvar::new(),
            entered: tokio::sync::Notify::new(),
            completed: std::sync::atomic::AtomicBool::new(false),
        });
        let mut registry = crate::registry::ExtensionRegistry::new();
        registry.register_event_handler(
            "waiting-observer".into(),
            EventTag::AgentStart,
            waiting_observer,
            (&*state as *const WaitingObserver).cast_mut().cast(),
        );
        let emitter = ExtensionEmitter::new(
            Arc::new(registry.snapshot()),
            crate::loader::PluginKeepalive::empty(),
        );
        let scope = Arc::new(rpi_agent::tool_execution_scope::ToolExecutionScope::default());
        let task_scope = Arc::clone(&scope);
        let dispatch = emitter.emit(AgentEvent::AgentStart);
        let task = tokio::spawn(async move { task_scope.run(dispatch).await });
        state.entered.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let mut idle = Box::pin(scope.wait_idle());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut idle)
                .await
                .is_err()
        );
        *state.released.lock().unwrap() = true;
        state.ready.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), idle)
            .await
            .unwrap();
        assert!(state.completed.load(Ordering::SeqCst));
    }

    #[test]
    fn emitter_dispatches_to_registered_handlers() {
        let _guard = HANDLER_TEST_LOCK.lock().unwrap();
        HANDLER_HITS.store(0, Ordering::SeqCst);
        let mut reg = crate::registry::ExtensionRegistry::new();
        reg.register_event_handler(
            "test-plugin".to_string(),
            EventTag::MessageEnd,
            counting_handler,
            std::ptr::null_mut(),
        );
        let snap = Arc::new(reg.snapshot());
        let emitter = ExtensionEmitter::new(snap, crate::loader::PluginKeepalive::empty());

        let am = AgentMessage::Assistant(Box::new(AssistantMessage {
            role: rpi_ai::types::AssistantRole,
            content: vec![rpi_ai::types::Content::text("hi")],
            api: rpi_ai::types::Api::AnthropicMessages,
            provider: "anthropic".to_string(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: rpi_ai::types::StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        }));
        // Use try_emit (sync) — no runtime needed.
        emitter.try_emit(AgentEvent::MessageEnd {
            message_id: None,
            message: am,
        });
        assert_eq!(HANDLER_HITS.load(Ordering::SeqCst), 1);

        // Stale registry → no dispatch.
        reg.invalidate();
        let am2 = AgentMessage::Assistant(Box::new(AssistantMessage {
            role: rpi_ai::types::AssistantRole,
            content: vec![rpi_ai::types::Content::text("hi")],
            api: rpi_ai::types::Api::AnthropicMessages,
            provider: "anthropic".to_string(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: rpi_ai::types::StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        }));
        emitter.try_emit(AgentEvent::MessageEnd {
            message_id: None,
            message: am2,
        });
        assert_eq!(
            HANDLER_HITS.load(Ordering::SeqCst),
            1,
            "stale registry must not dispatch"
        );
    }

    // --- TeeEmitter: fan-out to both the broadcast + the extension emitter ----

    struct ObservationOwner {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl rpi_agent::tool_program_observation::ToolProgramObserver for ObservationOwner {
        fn begin<'a>(
            &'a self,
            _child: rpi_agent::tool_program_observation::ToolProgramChild,
            _arguments: &'a serde_json::Value,
            _signal: tokio_util::sync::CancellationToken,
        ) -> BoxFuture<
            'a,
            Result<Arc<dyn rpi_agent::tool_program_observation::ToolProgramObservation>, String>,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err("reached genuine owner".into()) })
        }
    }
    struct ObservedEmitter(Arc<dyn rpi_agent::tool_program_observation::ToolProgramObserver>);
    impl AgentEmitter for ObservedEmitter {
        fn tool_program_observer(
            &self,
        ) -> Option<Arc<dyn rpi_agent::tool_program_observation::ToolProgramObserver>> {
            Some(self.0.clone())
        }
        fn emit(&self, _event: AgentEvent) -> BoxFuture<'static, ()> {
            Box::pin(async {})
        }
    }
    fn observation_child() -> rpi_agent::tool_program_observation::ToolProgramChild {
        rpi_agent::tool_program_observation::ToolProgramChild {
            root_id: "root".into(),
            root_name: "program".into(),
            parent_id: "root".into(),
            request_id: "tp-original".into(),
            name: "effect".into(),
            ordinal: 1,
        }
    }

    #[tokio::test]
    async fn tee_emitter_preserves_single_nested_and_shared_observer_authority() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let owner: Arc<dyn rpi_agent::tool_program_observation::ToolProgramObserver> =
            Arc::new(ObservationOwner {
                calls: calls.clone(),
            });
        let observed: Arc<dyn AgentEmitter> = Arc::new(ObservedEmitter(owner.clone()));
        let nested = TeeEmitter::new(vec![observed.clone()]);
        let tee = TeeEmitter::new(vec![
            Arc::new(rpi_agent::CollectorEmitter::default()),
            Arc::new(nested),
            observed,
        ]);
        let forwarded = tee.tool_program_observer().unwrap();
        assert!(Arc::ptr_eq(&owner, &forwarded));
        let error = forwarded
            .begin(
                observation_child(),
                &serde_json::json!({}),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .err()
            .unwrap();
        assert_eq!(error, "reached genuine owner");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(TeeEmitter::new(vec![]).tool_program_observer().is_none());
        assert!(
            TeeEmitter::new(vec![Arc::new(rpi_agent::CollectorEmitter::default())])
                .tool_program_observer()
                .is_none()
        );
    }

    #[tokio::test]
    async fn tee_emitter_refuses_distinct_observers_before_either_begin() {
        let first = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let second = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tee = TeeEmitter::new(vec![
            Arc::new(ObservedEmitter(Arc::new(ObservationOwner {
                calls: first.clone(),
            }))),
            Arc::new(ObservedEmitter(Arc::new(ObservationOwner {
                calls: second.clone(),
            }))),
        ]);
        let error = tee
            .tool_program_observer()
            .unwrap()
            .begin(
                observation_child(),
                &serde_json::json!({}),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .err()
            .unwrap();
        assert!(error.contains("multiple distinct"));
        assert_eq!(first.load(Ordering::SeqCst), 0);
        assert_eq!(second.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn tee_emitter_fans_out_to_every_child() {
        let _guard = HANDLER_TEST_LOCK.lock().unwrap();
        use rpi_agent::events::{AgentEmitter, CollectorEmitter};

        // Two collector emitters + record how many plugin handler hits land.
        let (collector_a, events_a) = CollectorEmitter::new();
        let (collector_b, events_b) = CollectorEmitter::new();
        HANDLER_HITS.store(0, Ordering::SeqCst);
        let mut reg = crate::registry::ExtensionRegistry::new();
        reg.register_event_handler(
            "test-plugin".to_string(),
            EventTag::MessageEnd,
            counting_handler,
            std::ptr::null_mut(),
        );
        let snap = Arc::new(reg.snapshot());
        let ext = ExtensionEmitter::new(snap, crate::loader::PluginKeepalive::empty());

        let tee = TeeEmitter::new(vec![
            Arc::new(collector_a),
            Arc::new(collector_b),
            Arc::new(ext),
        ]);

        let am = AgentMessage::Assistant(Box::new(AssistantMessage {
            role: rpi_ai::types::AssistantRole,
            content: vec![rpi_ai::types::Content::text("hi")],
            api: rpi_ai::types::Api::AnthropicMessages,
            provider: "anthropic".to_string(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage: Usage::zero(),
            stop_reason: rpi_ai::types::StopReason::Stop,
            deferred: None,
            error_message: None,
            raw_stop_reason: None,
            end_turn: None,
            timestamp: 0,
        }));
        // Broadcast path: both collectors receive the event, and the extension
        // emitter dispatches to the one registered handler.
        tee.emit(AgentEvent::MessageEnd {
            message_id: None,
            message: am,
        })
        .await;
        assert_eq!(
            events_a.lock().unwrap().len(),
            1,
            "collector A got the event"
        );
        assert_eq!(
            events_b.lock().unwrap().len(),
            1,
            "collector B got the event"
        );
        assert_eq!(
            HANDLER_HITS.load(Ordering::SeqCst),
            1,
            "plugin handler fired once"
        );
    }

    // --- dispatch_empty_event: no-payload lifecycle events (P0 session events)

    /// Verifies `dispatch_empty_event` fans a no-payload event (e.g. the
    /// rpi-specific `BeforeTuiStart` / session lifecycle tags) to every
    /// subscriber and reports whether any handler was invoked.
    #[test]
    fn dispatch_empty_event_fans_out_to_subscribers() {
        let _guard = HANDLER_TEST_LOCK.lock().unwrap();
        HANDLER_HITS.store(0, Ordering::SeqCst);
        let mut reg = crate::registry::ExtensionRegistry::new();
        reg.register_event_handler(
            "test-plugin".to_string(),
            EventTag::SessionStart,
            counting_handler,
            std::ptr::null_mut(),
        );
        reg.register_event_handler(
            "test-plugin".to_string(),
            EventTag::BeforeTuiStart,
            counting_handler,
            std::ptr::null_mut(),
        );
        let snap = Arc::new(reg.snapshot());

        // No subscriber for SessionShutdown → no handler invoked.
        assert!(!dispatch_empty_event(&snap, EventTag::SessionShutdown));
        assert_eq!(HANDLER_HITS.load(Ordering::SeqCst), 0);

        // SessionStart + BeforeTuiStart both have one subscriber each.
        assert!(dispatch_empty_event(&snap, EventTag::SessionStart));
        assert!(dispatch_empty_event(&snap, EventTag::BeforeTuiStart));
        assert_eq!(HANDLER_HITS.load(Ordering::SeqCst), 2);

        // Stale registry (session swapped): dispatch is a silent no-op inside
        // `dispatch_to_handlers` — no handler is invoked — though the fn still
        // reports subscribers existed (matches `dispatch_data_event` semantics).
        reg.invalidate();
        assert!(dispatch_empty_event(&snap, EventTag::SessionStart));
        assert!(dispatch_empty_event(&snap, EventTag::BeforeTuiStart));
        assert_eq!(HANDLER_HITS.load(Ordering::SeqCst), 2);
    }

    // --- dispatch_lifecycle_event: veto + timeout (P1) ---

    /// A lifecycle handler that vetoes (returns the SDK abort code).
    extern "C" fn aborting_handler(_ev: StablePluginEvent, _ud: *mut std::ffi::c_void) -> i32 {
        rpi_plugin_sdk::EVENT_HANDLER_ABORT
    }

    /// A lifecycle handler that returns success (`0`) — no veto.
    extern "C" fn continue_handler(_ev: StablePluginEvent, _ud: *mut std::ffi::c_void) -> i32 {
        rpi_plugin_sdk::EVENT_HANDLER_CONTINUE
    }

    /// A lifecycle handler that returns a generic handled error — no veto.
    extern "C" fn error_handler(_ev: StablePluginEvent, _ud: *mut std::ffi::c_void) -> i32 {
        rpi_plugin_sdk::EVENT_HANDLER_ERROR
    }

    #[test]
    fn lifecycle_timeout_budget_matches_spec() {
        assert_eq!(
            lifecycle_timeout_for(EventTag::BeforeTuiStart),
            std::time::Duration::from_secs(5)
        );
        assert_eq!(
            lifecycle_timeout_for(EventTag::SessionShutdown),
            std::time::Duration::from_secs(15)
        );
        assert_eq!(
            lifecycle_timeout_for(EventTag::SessionStart),
            std::time::Duration::from_secs(10)
        );
    }

    /// A handler returning `EVENT_HANDLER_ABORT` vetoes the phase; the returned
    /// reason names the owning extension.
    #[tokio::test]
    async fn dispatch_lifecycle_event_honors_veto() {
        let mut reg = crate::registry::ExtensionRegistry::new();
        reg.register_event_handler(
            "veto-ext".to_string(),
            EventTag::BeforeTuiStart,
            aborting_handler,
            std::ptr::null_mut(),
        );
        let snap = Arc::new(reg.snapshot());
        let result = dispatch_lifecycle_event(
            &snap,
            EventTag::BeforeTuiStart,
            crate::PluginKeepalive::empty(),
        )
        .await;
        assert_eq!(
            result.as_deref(),
            Some("extension `veto-ext` vetoed BeforeTuiStart")
        );
    }

    /// A `0` (continue) or generic nonzero (handled error) return does NOT veto;
    /// the fan-out still visits every handler and returns `None`.
    #[tokio::test]
    async fn dispatch_lifecycle_event_continue_and_error_do_not_veto() {
        let mut reg = crate::registry::ExtensionRegistry::new();
        reg.register_event_handler(
            "ok-ext".to_string(),
            EventTag::SessionStart,
            continue_handler,
            std::ptr::null_mut(),
        );
        reg.register_event_handler(
            "err-ext".to_string(),
            EventTag::SessionStart,
            error_handler,
            std::ptr::null_mut(),
        );
        let snap = Arc::new(reg.snapshot());
        assert!(dispatch_lifecycle_event(
            &snap,
            EventTag::SessionStart,
            crate::PluginKeepalive::empty()
        )
        .await
        .is_none());
    }

    fn tool_call_session(reg: &crate::ExtensionRegistry) -> crate::ExtensionSession {
        crate::ExtensionSession::from_parts(
            Arc::new(reg.snapshot()),
            crate::PluginKeepalive::empty(),
            Vec::new(),
            None,
        )
    }

    #[tokio::test]
    async fn tool_call_approval_error_and_stale_session_block_execution() {
        let mut reg = crate::ExtensionRegistry::new();
        reg.register_event_handler(
            "broken-gate".into(),
            EventTag::ToolCall,
            error_handler,
            std::ptr::null_mut(),
        );
        let session = tool_call_session(&reg);
        let signal = tokio_util::sync::CancellationToken::new();
        let ui = crate::UiDialogMailbox::new();
        let reason =
            dispatch_tool_call_event(&session, "c1", "bash", &serde_json::json!({}), &signal, &ui)
                .await
                .unwrap();
        assert!(reason.contains("approval failed: rc="), "{reason}");
        reg.invalidate();
        let reason =
            dispatch_tool_call_event(&session, "c2", "bash", &serde_json::json!({}), &signal, &ui)
                .await
                .unwrap();
        assert!(reason.contains("stale"), "{reason}");
    }

    struct WaitingApproval {
        ui: crate::UiDialogMailbox,
        entered: tokio::sync::Notify,
        finished: tokio::sync::Notify,
        release: std::sync::atomic::AtomicBool,
    }

    static APPROVAL_CONTEXTS: rpi_plugin_sdk::handles::ToolHandles<Arc<WaitingApproval>> =
        rpi_plugin_sdk::handles::ToolHandles::new();

    rpi_plugin_sdk::event_handler! { fn waiting_approval(event, data) {
        let Some(context) = APPROVAL_CONTEXTS.get(data) else {
            return rpi_plugin_sdk::EVENT_HANDLER_ERROR;
        };
        let state = context.lock().clone();
        let call_id = event.tool_call().expect("tool call event").tool_call_id;
        state
            .ui
            .handle(serde_json::json!({
                "op": "open", "requestId": "approval", "toolCallId": call_id,
                "ui": {"question": "Allow?"},
            }))
            .unwrap();
        state.entered.notify_one();
        loop {
            if state.release.load(Ordering::SeqCst) {
                break;
            }
            if state.ui.poll("approval").unwrap()["status"] == "cancelled" {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // Stay inside the callback so the test can check library retention
        // after the async dispatch has already returned.
        while !state.release.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        state.finished.notify_one();
        rpi_plugin_sdk::EVENT_HANDLER_ABORT
    } }

    async fn check_interrupted_approval(cancel: bool) {
        let ui = crate::UiDialogMailbox::new();
        ui.attach();
        ui.handle(serde_json::json!({
            "requestId": "other", "toolCallId": "other-call", "ui": {},
        }))
        .unwrap();
        let state = Arc::new(WaitingApproval {
            ui: ui.clone(),
            entered: tokio::sync::Notify::new(),
            finished: tokio::sync::Notify::new(),
            release: std::sync::atomic::AtomicBool::new(false),
        });
        // Release on failed assertions too, so the blocking runtime can exit.
        struct ReleaseWorker(Arc<WaitingApproval>, usize);
        impl Drop for ReleaseWorker {
            fn drop(&mut self) {
                self.0.release.store(true, Ordering::SeqCst);
                drop(APPROVAL_CONTEXTS.remove(self.1 as rpi_plugin_sdk::StepHandle));
            }
        }
        let context = APPROVAL_CONTEXTS.insert(state.clone());
        let _release = ReleaseWorker(state.clone(), context as usize);
        let mut reg = crate::ExtensionRegistry::new();
        reg.register_event_handler(
            "waiting-gate".into(),
            EventTag::ToolCall,
            waiting_approval,
            context,
        );
        let keepalive = crate::PluginKeepalive::empty();
        let weak = Arc::downgrade(&keepalive);
        let session = crate::ExtensionSession::from_parts(
            Arc::new(reg.snapshot()),
            keepalive,
            Vec::new(),
            None,
        );
        let signal = tokio_util::sync::CancellationToken::new();
        let task_signal = signal.clone();
        let task_ui = ui.clone();
        let task = tokio::spawn(async move {
            dispatch_tool_call_event_with_timeout(
                &session,
                "call",
                "bash",
                &serde_json::json!({}),
                &task_signal,
                &task_ui,
                if cancel {
                    TOOL_CALL_TIMEOUT
                } else {
                    std::time::Duration::from_millis(200)
                },
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), state.entered.notified())
            .await
            .unwrap();
        if cancel {
            signal.cancel();
        }
        let reason = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            reason.contains(if cancel { "cancelled" } else { "timed out" }),
            "{reason}"
        );
        assert!(
            weak.upgrade().is_some(),
            "running callback must keep its library mapped"
        );
        assert_eq!(ui.poll("other").unwrap()["status"], "pending");
        state.release.store(true, Ordering::SeqCst);
        tokio::time::timeout(std::time::Duration::from_secs(2), state.finished.notified())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn tool_call_approval_cancel_releases_waiter_and_retains_running_library() {
        check_interrupted_approval(true).await;
    }

    #[tokio::test]
    async fn tool_call_approval_timeout_blocks_and_retains_running_library() {
        check_interrupted_approval(false).await;
    }

    // --- dispatch_tool_call_event: veto (B3b before_tool_call) ---

    /// A `ToolCall` handler that vetoes blocks the tool and names the tool.
    #[tokio::test]
    async fn dispatch_tool_call_event_honors_veto() {
        let mut reg = crate::registry::ExtensionRegistry::new();
        reg.register_event_handler(
            "gate-ext".to_string(),
            EventTag::ToolCall,
            aborting_handler,
            std::ptr::null_mut(),
        );
        let session = tool_call_session(&reg);
        let veto = dispatch_tool_call_event(
            &session,
            "call_1",
            "bash",
            &serde_json::json!({"command": "rm -rf /"}),
            &tokio_util::sync::CancellationToken::new(),
            &crate::UiDialogMailbox::new(),
        )
        .await;
        assert_eq!(
            veto.as_deref(),
            Some("extension `gate-ext` blocked tool `bash`")
        );
    }

    /// A continue code does not veto; no subscribers is also `None`.
    #[tokio::test]
    async fn dispatch_tool_call_event_continue_and_empty_do_not_veto() {
        let mut reg = crate::registry::ExtensionRegistry::new();
        reg.register_event_handler(
            "ok-ext".to_string(),
            EventTag::ToolCall,
            continue_handler,
            std::ptr::null_mut(),
        );
        let session = tool_call_session(&reg);
        assert!(dispatch_tool_call_event(
            &session,
            "call_1",
            "read",
            &serde_json::json!({"path": "x"}),
            &tokio_util::sync::CancellationToken::new(),
            &crate::UiDialogMailbox::new(),
        )
        .await
        .is_none());

        let empty = crate::registry::ExtensionRegistry::new();
        let empty_session = tool_call_session(&empty);
        assert!(dispatch_tool_call_event(
            &empty_session,
            "call_2",
            "read",
            &serde_json::json!({}),
            &tokio_util::sync::CancellationToken::new(),
            &crate::UiDialogMailbox::new()
        )
        .await
        .is_none());
    }

    /// No subscribers → no veto, no work.
    #[tokio::test]
    async fn dispatch_lifecycle_event_no_subscribers_is_none() {
        let reg = crate::registry::ExtensionRegistry::new();
        let snap = Arc::new(reg.snapshot());
        assert!(dispatch_lifecycle_event(
            &snap,
            EventTag::BeforeTuiStart,
            crate::PluginKeepalive::empty()
        )
        .await
        .is_none());
    }

    /// A stale registry (swapped-out session) is a no-op — a veto from a stale
    /// handler must NOT abort the new session's startup.
    #[tokio::test]
    async fn dispatch_lifecycle_event_stale_is_none() {
        let mut reg = crate::registry::ExtensionRegistry::new();
        reg.register_event_handler(
            "veto-ext".to_string(),
            EventTag::BeforeTuiStart,
            aborting_handler,
            std::ptr::null_mut(),
        );
        let snap = Arc::new(reg.snapshot());
        reg.invalidate();
        assert!(dispatch_lifecycle_event(
            &snap,
            EventTag::BeforeTuiStart,
            crate::PluginKeepalive::empty()
        )
        .await
        .is_none());
    }

    /// The observe fan-out records one JSONL line per handler when the global
    /// event logger is enabled.
    #[test]
    fn dispatch_records_handler_invocation_to_event_log() {
        let _guard = HANDLER_TEST_LOCK.lock().unwrap();
        HANDLER_HITS.store(0, Ordering::SeqCst);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        crate::event_log::set_event_logger_for_test(crate::event_log::EventLogger::open(
            path.clone(),
        ));

        let mut reg = crate::registry::ExtensionRegistry::new();
        reg.register_event_handler(
            "log-ext".to_string(),
            EventTag::MessageEnd,
            counting_handler,
            std::ptr::null_mut(),
        );
        let snap = Arc::new(reg.snapshot());
        let event = StablePluginEvent::message(
            EventTag::MessageEnd,
            StbString::from_string("m".to_string()),
        );
        dispatch_to_handlers(&snap, &event);

        // Restore the disabled default so later tests don't write into the
        // (soon-deleted) temp file.
        crate::event_log::set_event_logger_for_test(crate::event_log::EventLogger::disabled());

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""event":"MessageEnd""#), "log: {text}");
        assert!(text.contains(r#""plugin":"log-ext""#), "log: {text}");
        assert!(text.contains(r#""result":"Continue""#), "log: {text}");
        assert_eq!(HANDLER_HITS.load(Ordering::SeqCst), 1);
    }

    // --- key-routing (`dispatch_data_event_claiming`) ----------------------

    /// Handler that claims the event unless `CLAIM_ROUTING` says otherwise.
    /// `CLAIM_ROUTING` is global because an `extern "C" fn` cannot capture.
    static CLAIM_ROUTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

    rpi_plugin_sdk::event_handler! { fn claiming_handler(ev, _ud) {
        // The payload must be readable as a data event (this is also what
        // exercises the `Input` free path not leaking/aliasing).
        let seen = ev.data_text().expect("input data event");
        assert!(seen.contains("space"), "payload not delivered: {seen}");
        if CLAIM_ROUTING.load(Ordering::SeqCst) {
            rpi_plugin_sdk::EVENT_HANDLER_CLAIMED
        } else {
            rpi_plugin_sdk::EVENT_HANDLER_CONTINUE
        }
    } }

    fn input_snapshot(with_handler: bool) -> Arc<crate::registry::RegistrySnapshot> {
        let mut reg = crate::registry::ExtensionRegistry::new();
        if with_handler {
            reg.register_event_handler(
                "voice".to_string(),
                EventTag::Input,
                claiming_handler,
                std::ptr::null_mut(),
            );
        }
        Arc::new(reg.snapshot())
    }

    #[test]
    fn claiming_dispatch_reports_claim_and_passthrough() {
        let _guard = HANDLER_TEST_LOCK.lock().unwrap();
        let snap = input_snapshot(true);

        CLAIM_ROUTING.store(true, Ordering::SeqCst);
        assert!(
            dispatch_data_event_claiming(&snap, EventTag::Input, r#"{"key":"space"}"#),
            "a CLAIMED handler must swallow the key"
        );

        CLAIM_ROUTING.store(false, Ordering::SeqCst);
        assert!(
            !dispatch_data_event_claiming(&snap, EventTag::Input, r#"{"key":"space"}"#),
            "a CONTINUE handler must let the key reach the editor"
        );
        CLAIM_ROUTING.store(true, Ordering::SeqCst);
    }

    #[test]
    fn claiming_dispatch_without_subscribers_is_false() {
        let snap = input_snapshot(false);
        assert!(!dispatch_data_event_claiming(
            &snap,
            EventTag::Input,
            r#"{"key":"space"}"#
        ));
    }

    // --- editor-change (`dispatch_data_event`) -----------------------------

    /// The `EditorChange` notification reaches subscribers with its payload
    /// intact and returns "no veto" (it is an observer event: the draft change
    /// has already happened and must never be blocked).
    #[test]
    fn editor_change_reaches_subscribers_with_its_payload() {
        let _guard = HANDLER_TEST_LOCK.lock().unwrap();
        let mut reg = crate::registry::ExtensionRegistry::new();
        reg.register_event_handler(
            "voice".to_string(),
            EventTag::EditorChange,
            editor_change_handler,
            std::ptr::null_mut(),
        );
        let snap = Arc::new(reg.snapshot());

        EDITOR_CHANGE_SEEN.store(false, Ordering::SeqCst);
        assert!(
            dispatch_data_event(
                &snap,
                EventTag::EditorChange,
                r#"{"chars":4,"empty":false}"#
            ),
            "a subscribed handler must be invoked"
        );
        assert!(
            EDITOR_CHANGE_SEEN.load(Ordering::SeqCst),
            "payload missing or handler not called"
        );
    }

    // Handler that records it saw a well-formed editor-change payload. The
    // flag is global because an `extern "C" fn` cannot capture.
    static EDITOR_CHANGE_SEEN: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    rpi_plugin_sdk::event_handler! { fn editor_change_handler(ev, _ud) {
        // Reading the data payload here also exercises the `EditorChange` free
        // path (a wrong arm in `free_dispatched_event` would corrupt/leak).
        let seen = ev.data_text().expect("editor change data event");
        if seen.contains("chars") && seen.contains("empty") {
            EDITOR_CHANGE_SEEN.store(true, Ordering::SeqCst);
        }
        rpi_plugin_sdk::EVENT_HANDLER_CONTINUE
    } }
}
