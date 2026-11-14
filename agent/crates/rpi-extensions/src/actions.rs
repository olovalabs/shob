//! B5a — the plugin→host `runtime_action` bridge (inverted FFI).
//!
//! A plugin invokes `PluginApi::runtime_action` to drive the harness (send a
//! message, switch models, fork a session, reload extensions, …). Unlike the
//! register trampolines — which run synchronously inside the selected register
//! entrypoint
//! and recover host state via the thread-local `CURRENT_HOST_API` —
//! `runtime_action` is called post-register, either from a host tool driver or
//! a foreign worker. `PluginApi::user_data` carries an opaque, non-reused handle
//! resolved through a weak registry, never a dereferenceable bridge address.
//! Expired registrations fail safely, including candidate reload rollback.
//!
//! Synchronous tool callbacks inherit a host-owned invocation via a scoped
//! thread-local; workers forward the invocation handle and capability injected
//! into `__rpi`. ChildAgent rejects registration-only, cancelled and child
//! invocations. A cached registration cannot override a child's driver scope.
//!
//! [`ActionBridge`] captures a Tokio [`Handle`] at construction. Dispatch can
//! therefore spawn from foreign threads and return over a standard synchronous
//! channel without depending on the plugin thread having a Tokio runtime.
//!
//! ## Cycle-free leaf DAG
//!
//! [`RuntimeActionHost`] is defined here (NOT in `rpi-harness`) so
//! `rpi-extensions` stays a leaf: the trait names only JSON + primitives — no
//! `rpi-harness` types cross. The host impl (`HarnessActionHost`) lives in
//! `rpi-cli`, where it can name the harness freely; `rpi-extensions` only
//! carries the async surface. This preserves the documented DAG
//! (`lib.rs:10-16`: `rpi-extensions` does NOT depend on `rpi-harness`).

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock, Weak};

// Opaque, non-reused IDs, never addresses. A copied PluginApi may outlive its
// registration (e.g. a failed reload); resolving it must not dereference freed memory.
struct RegistryEntry {
    bridge: Weak<ActionBridge>,
    active: Weak<AtomicBool>,
}
static BRIDGES: OnceLock<Mutex<HashMap<usize, RegistryEntry>>> = OnceLock::new();
fn bridges() -> &'static Mutex<HashMap<usize, RegistryEntry>> {
    BRIDGES.get_or_init(Mutex::default)
}
thread_local! {
    static CURRENT_INVOCATION: std::cell::RefCell<Option<Arc<ActionBridge>>> = const { std::cell::RefCell::new(None) };
}
pub(crate) struct InvocationGuard {
    previous: Option<Arc<ActionBridge>>,
    _thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl Drop for InvocationGuard {
    fn drop(&mut self) {
        CURRENT_INVOCATION.with(|slot| *slot.borrow_mut() = self.previous.take());
    }
}

use rpi_plugin_sdk::{RuntimeActionId, StbString, StbStringRef};
use tokio::runtime::Handle;

/// Prefix a plugin uses to mark a `requestId` as **not the TUI's to consume**.
///
/// Why it exists: the interactive TUI and plugins share one [`UiDialogMailbox`].
/// Without this convention a plugin's question (a remote UI's permission gate,
/// say) gets taken by the TUI and drawn in the terminal — while the user is
/// looking at a remote client, which sees nothing.
///
/// Paired with [`UiDialogMailbox::take_pending_filtered`] (the TUI passes
/// `|r| !is_reserved(&r.request_id)`).
pub const PLUGIN_RESERVED_PREFIX: &str = "pidingo:";

/// A pending host UI request emitted by a plugin runtime action.
///
/// The request and tool-call ids are intentionally separate: a single tool
/// call may ask more than one question, while concurrent tool calls must never
/// consume one another's answers. `raw` retains the complete action envelope;
/// `ui` is the normalized UI payload that TUI renderers generally need.
#[derive(Clone, Debug, PartialEq)]
pub struct UiDialogRequest {
    pub request_id: String,
    pub tool_call_id: Option<String>,
    pub ui: serde_json::Value,
    pub raw: serde_json::Value,
}

struct UiDialogEntry {
    request: UiDialogRequest,
    answer: Option<serde_json::Value>,
    cancelled: bool,
}

struct UiDialogState {
    attached: bool,
    /// An external UI (a relay) has taken over dialog delivery.
    ///
    /// While set, the local TUI must not take requests: both consumers draw
    /// from the same `pending` queue, so whoever runs first wins and the other
    /// never sees the question. With a remote client attached, the question has
    /// to reach *that* client — the terminal is not where the user is looking.
    external_takeover: bool,
    pending: VecDeque<String>,
    active: HashMap<String, UiDialogEntry>,
    cancelled_tool_calls: std::collections::HashSet<String>,
}

/// Thread-safe mailbox connecting plugin runtime actions to an interactive
/// host (normally the TUI). It deliberately carries only JSON and primitives,
/// so the extension crate remains independent of the UI crate.
#[derive(Clone)]
pub struct UiDialogMailbox {
    state: Arc<Mutex<UiDialogState>>,
}

impl Default for UiDialogMailbox {
    fn default() -> Self {
        Self::new()
    }
}

impl UiDialogMailbox {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(UiDialogState {
                attached: false,
                external_takeover: false,
                pending: VecDeque::new(),
                active: HashMap::new(),
                cancelled_tool_calls: std::collections::HashSet::new(),
            })),
        }
    }

    /// Mark an interactive consumer as available. Re-attaching is idempotent.
    pub fn attach(&self) {
        self.state
            .lock()
            .expect("ui dialog mailbox poisoned")
            .attached = true;
    }

    /// Hand dialog delivery to an external UI (or take it back).
    ///
    /// The TUI and a relay both call "take"; without an owner, whichever polls
    /// first wins and the other never sees the request — a third-party
    /// extension's question can end up drawn in a terminal nobody is watching.
    /// A caller that owns the user's attention (a remote client) sets this, and
    /// the local TUI then skips taking entirely.
    ///
    /// Idempotent, and independent of [`Self::attach`]: the TUI stays "attached"
    /// while a remote client is in charge, it just does not consume.
    pub fn set_external_takeover(&self, takeover: bool) {
        self.state
            .lock()
            .expect("ui dialog mailbox poisoned")
            .external_takeover = takeover;
    }

    /// Whether an external UI currently owns dialog delivery.
    pub fn external_takeover(&self) -> bool {
        self.state
            .lock()
            .expect("ui dialog mailbox poisoned")
            .external_takeover
    }

    /// Detach the consumer and cancel every outstanding request. This prevents
    /// a TUI restart/reload from leaving plugin polls blocked forever.
    pub fn detach(&self) {
        let mut state = self.state.lock().expect("ui dialog mailbox poisoned");
        state.attached = false;
        // Release ownership with the consumer. A stale takeover flag would make
        // the TUI refuse every later dialog, so losing the relay client would
        // leave the user unable to answer anything.
        state.external_takeover = false;
        state.pending.clear();
        for entry in state.active.values_mut() {
            entry.cancelled = true;
            entry.answer = None;
        }
    }

    /// Give dialog delivery back to the local TUI.
    ///
    /// Requests already taken by the relay but never answered are put back in
    /// the queue so the TUI draws them. They are not answered or cancelled
    /// here: fabricating a reply for a question nobody read would be worse than
    /// asking it again.
    ///
    /// Returns the number of requests handed back.
    pub fn release_takeover(&self) -> usize {
        let mut state = self.state.lock().expect("ui dialog mailbox poisoned");
        state.external_takeover = false;
        let mut requeued: Vec<String> = state
            .active
            .iter()
            .filter(|(_, entry)| !entry.cancelled && entry.answer.is_none())
            .map(|(request_id, _)| request_id.clone())
            .filter(|request_id| !state.pending.contains(request_id))
            .collect();
        if requeued.is_empty() {
            return 0;
        }
        let count = requeued.len();
        // Stable order so a restored queue does not reshuffle the prompts.
        requeued.sort();
        for request_id in requeued.into_iter().rev() {
            state.pending.push_front(request_id);
        }
        count
    }

    pub fn is_attached(&self) -> bool {
        self.state
            .lock()
            .expect("ui dialog mailbox poisoned")
            .attached
    }

    /// Remove and return the next request for presentation by the TUI.
    /// Cancelled/answered requests are skipped; duplicate `open` calls never
    /// enqueue the same request id twice.
    pub fn take_pending(&self) -> Option<UiDialogRequest> {
        self.take_pending_filtered(|_| true)
    }

    /// Take a request for the local TUI, but only while no external UI owns
    /// delivery.
    ///
    /// The takeover check and the dequeue must be one critical section. Checking
    /// `external_takeover()` and then calling [`Self::take_pending_filtered`]
    /// acquires the lock twice, and a relay can claim ownership in between: the
    /// TUI would then dequeue a question meant for the remote client, so nobody
    /// the user is watching ever sees it.
    pub fn take_pending_unless_external<F>(&self, keep: F) -> Option<UiDialogRequest>
    where
        F: Fn(&UiDialogRequest) -> bool,
    {
        let mut state = self.state.lock().expect("ui dialog mailbox poisoned");
        if state.external_takeover {
            return None;
        }
        Self::take_pending_locked(&mut state, keep)
    }

    /// Like [`take_pending`], but only takes a request `keep` accepts; the rest
    /// **stay queued** for whoever they belong to.
    ///
    /// The TUI and plugins race on the same mailbox. When a plugin marks a
    /// request as belonging to a host consumer (a remote UI's gate), the TUI
    /// must not swallow it: the user would never see the question in the remote
    /// client, while a duplicate pops up in the terminal.
    ///
    /// ## Reserved-prefix convention
    ///
    /// A `requestId` starting with [`PLUGIN_RESERVED_PREFIX`] is never taken by
    /// the TUI. `take_pending()` takes everything (it passes `|_| true`), so
    /// skipping reserved entries has to be explicit:
    /// `take_pending_filtered(|r| !self.is_reserved(&r.request_id))`.
    pub fn take_pending_filtered<F>(&self, keep: F) -> Option<UiDialogRequest>
    where
        F: Fn(&UiDialogRequest) -> bool,
    {
        let mut state = self.state.lock().expect("ui dialog mailbox poisoned");
        Self::take_pending_locked(&mut state, keep)
    }

    fn take_pending_locked<F>(
        state: &mut std::sync::MutexGuard<'_, UiDialogState>,
        keep: F,
    ) -> Option<UiDialogRequest>
    where
        F: Fn(&UiDialogRequest) -> bool,
    {
        let mut scanned = 0usize;
        let total = state.pending.len();
        while scanned < total {
            scanned += 1;
            let Some(request_id) = state.pending.pop_front() else {
                break;
            };
            let Some(entry) = state.active.get(&request_id) else {
                continue;
            };
            if entry.cancelled || entry.answer.is_some() {
                continue;
            }
            let request = entry.request.clone();
            if keep(&request) {
                return Some(request);
            }
            // Not mine — put it back for whoever should handle it.
            state.pending.push_back(request_id);
        }
        None
    }

    /// Take **every request no plugin reserved** (the `RelayUiDialog` `take`).
    ///
    /// For "a remote UI takes over": while an external client is attached, a
    /// third-party extension's question should be forwarded there instead of
    /// popping up in the terminal. Reserved entries stay behind.
    ///
    /// Taken requests leave `pending` but stay in `active`, so a later
    /// `respond` / `cancel` with the same `requestId` still resolves them.
    pub fn take_all_unreserved(&self) -> Vec<UiDialogRequest> {
        let mut state = self.state.lock().expect("ui dialog mailbox poisoned");
        let mut out: Vec<UiDialogRequest> = Vec::new();
        // One pass does both: drain everything, then push back what stays.
        let drained = std::mem::take(&mut state.pending);
        for request_id in drained {
            let Some(entry) = state.active.get(&request_id) else {
                continue;
            };
            if entry.cancelled || entry.answer.is_some() {
                continue;
            }
            if request_id.starts_with(PLUGIN_RESERVED_PREFIX) {
                state.pending.push_back(request_id);
            } else {
                out.push(entry.request.clone());
            }
        }
        out
    }

    /// Whether `request_id` is reserved by a plugin (the TUI must not consume it).
    pub fn is_reserved(&self, request_id: &str) -> bool {
        request_id.starts_with(PLUGIN_RESERVED_PREFIX)
    }

    pub fn pending_len(&self) -> usize {
        self.state
            .lock()
            .expect("ui dialog mailbox poisoned")
            .pending
            .len()
    }

    /// Inspect lifecycle without consuming the answer reserved for a plugin.
    pub fn is_pending(&self, request_id: &str) -> bool {
        self.state
            .lock()
            .expect("ui dialog mailbox poisoned")
            .active
            .get(request_id)
            .is_some_and(|entry| !entry.cancelled && entry.answer.is_none())
    }

    /// Supply an answer for a request. Repeating the same operation is
    /// idempotent; answering a cancelled/unknown request is an explicit error.
    pub fn respond(&self, request_id: &str, answer: serde_json::Value) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "ui dialog mailbox poisoned".to_string())?;
        let entry = state
            .active
            .get_mut(request_id)
            .ok_or_else(|| format!("unknown UI request `{request_id}`"))?;
        if entry.cancelled {
            return Err(format!("UI request `{request_id}` was cancelled"));
        }
        if entry.answer.is_none() {
            entry.answer = Some(answer);
        }
        Ok(())
    }

    /// Mark one request cancelled. The terminal state remains visible to the
    /// plugin's next `poll`, then is reclaimed by that poll.
    pub fn cancel(&self, request_id: &str) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "ui dialog mailbox poisoned".to_string())?;
        let entry = state
            .active
            .get_mut(request_id)
            .ok_or_else(|| format!("unknown UI request `{request_id}`"))?;
        entry.cancelled = true;
        entry.answer = None;
        state.pending.retain(|id| id != request_id);
        Ok(())
    }

    /// Cancel all requests currently known to the mailbox.
    pub fn cancel_all(&self) {
        let mut state = self.state.lock().expect("ui dialog mailbox poisoned");
        state.pending.clear();
        for entry in state.active.values_mut() {
            entry.cancelled = true;
            entry.answer = None;
        }
    }

    /// Cancel approval requests belonging to one tool without disturbing
    /// questions from concurrent tool calls.
    pub fn cancel_tool_call(&self, tool_call_id: &str) {
        let mut state = self.state.lock().expect("ui dialog mailbox poisoned");
        // A callback can reach `open` just after the host cancels its call.
        // Remember the cancellation so it cannot enqueue an orphan question.
        state.cancelled_tool_calls.insert(tool_call_id.to_string());
        for entry in state.active.values_mut() {
            if entry.request.tool_call_id.as_deref() == Some(tool_call_id) {
                entry.cancelled = true;
                entry.answer = None;
            }
        }
        let cancelled: std::collections::HashSet<String> = state
            .active
            .iter()
            .filter(|(_, entry)| entry.cancelled)
            .map(|(id, _)| id.clone())
            .collect();
        state.pending.retain(|id| !cancelled.contains(id));
    }

    /// Poll a request and consume a terminal answer/cancellation. Pending
    /// polls are cheap and preserve the request for subsequent calls.
    pub fn poll(&self, request_id: &str) -> Result<serde_json::Value, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "ui dialog mailbox poisoned".to_string())?;
        let terminal = match state.active.get(request_id) {
            Some(entry) if entry.cancelled => Some(serde_json::json!({
                "status": "cancelled",
                "requestId": request_id,
                "toolCallId": entry.request.tool_call_id,
            })),
            Some(entry) if entry.answer.is_some() => Some(serde_json::json!({
                "status": "answered",
                "requestId": request_id,
                "toolCallId": entry.request.tool_call_id,
                "answer": entry.answer.clone().unwrap_or(serde_json::Value::Null),
            })),
            Some(entry) => Some(serde_json::json!({
                "status": "pending",
                "requestId": request_id,
                "toolCallId": entry.request.tool_call_id,
            })),
            None => None,
        };
        let Some(result) = terminal else {
            return Err(format!("unknown UI request `{request_id}`"));
        };
        if result.get("status").and_then(serde_json::Value::as_str) != Some("pending") {
            state.active.remove(request_id);
        }
        Ok(result)
    }

    /// Parse and execute the JSON action envelope used by runtime action 17.
    /// Supported operations are `open`, `poll`, `cancel`; `response` is also
    /// accepted as a convenience for hosts that proxy answers through JSON.
    pub fn handle(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let op = args
            .get("op")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("open");
        let request_id = || {
            args.get("requestId")
                .or_else(|| args.get("request_id"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| "missing non-empty `requestId`".to_string())
        };
        match op {
            "open" => {
                let id = request_id()?;
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| "ui dialog mailbox poisoned".to_string())?;
                if !state.attached {
                    return Err("ask_user requires an interactive UI".to_string());
                }
                if let Some(entry) = state.active.get(&id) {
                    return Ok(if entry.cancelled {
                        serde_json::json!({"status":"cancelled","requestId":id})
                    } else if let Some(answer) = &entry.answer {
                        serde_json::json!({"status":"answered","requestId":id,"answer":answer})
                    } else {
                        serde_json::json!({"status":"pending","requestId":id})
                    });
                }
                let tool_call_id = args
                    .get("toolCallId")
                    .or_else(|| args.get("tool_call_id"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                if tool_call_id
                    .as_ref()
                    .is_some_and(|id| state.cancelled_tool_calls.contains(id))
                {
                    return Ok(serde_json::json!({
                        "status": "cancelled", "requestId": id, "toolCallId": tool_call_id,
                    }));
                }
                let ui = args.get("ui").cloned().unwrap_or_else(|| args.clone());
                rpi_plugin_sdk::dialogs::validate(&ui, tool_call_id.as_deref())?;
                state.active.insert(
                    id.clone(),
                    UiDialogEntry {
                        request: UiDialogRequest {
                            request_id: id.clone(),
                            tool_call_id: tool_call_id.clone(),
                            ui,
                            raw: args,
                        },
                        answer: None,
                        cancelled: false,
                    },
                );
                state.pending.push_back(id.clone());
                Ok(serde_json::json!({"status":"pending","requestId":id,"toolCallId":tool_call_id}))
            }
            "poll" => self.poll(&request_id()?),
            "cancel" => {
                let id = request_id()?;
                self.cancel(&id)?;
                Ok(serde_json::json!({"status":"cancelled","requestId":id}))
            }
            "response" | "respond" => {
                let id = request_id()?;
                let answer = args
                    .get("answer")
                    .or_else(|| args.get("value"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                self.respond(&id, answer)?;
                Ok(serde_json::json!({"status":"answered","requestId":id}))
            }
            other => Err(format!("unknown UI dialog operation `{other}`")),
        }
    }
}

/// Host-only, owner-bound UI writer. It contains no plugin callbacks or pointers.
pub type SideQuestionSink =
    Arc<dyn Fn(serde_json::Value) -> Result<serde_json::Value, String> + Send + Sync>;

/// The host-side implementation the bridge delegates to. Defined in
/// `rpi-extensions` (NOT `rpi-harness`) so the crate DAG stays a leaf: this is a
/// trait the **host** (`rpi-cli`) implements over the harness — `rpi-extensions`
/// only names the async surface + carries JSON params/results. No `rpi-harness`
/// types appear in the trait.
///
/// Each method corresponds to one [`RuntimeActionId`] variant. Complex args
/// arrive as parsed JSON (`serde_json::Value`); complex results return as
/// `serde_json::Value`. The host maps its native types (Model, AgentMessage, …)
/// to/from JSON at the impl boundary. Errors are `String` (become the action's
/// nonzero `i32` + `{"error": msg}` JSON on the plugin side).
///
/// `dispatch` below connects the FFI id to the method; owner-scoped actions
/// are routed directly by the plugin bridge.
#[async_trait::async_trait]
pub trait RuntimeActionHost: Send + Sync {
    /// Action 30: an authenticated host-owned executing invocation, never registry JSON.
    async fn tool_program(
        &self,
        capability: Arc<rpi_agent::tool_program::ToolProgram>,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        capability.handle(args).await
    }
    /// Revoke side questions when their plugin bridge is retired.
    fn cancel_side_questions(&self, _owner: u64) {}
    /// Host-internal TUI snapshots; never exposed as a cross-owner plugin action.
    fn side_question_revision(&self) -> u64 {
        0
    }
    fn side_question_views(&self) -> Vec<serde_json::Value> {
        Vec::new()
    }
    async fn side_question_ui(
        &self,
        _owner: u64,
        _thread_id: String,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        Err("SideQuestion UI is not supported by this host".into())
    }
    /// Owner-scoped read-only inference, including host-owned UI publication.
    async fn side_question(
        &self,
        _owner: u64,
        _args: serde_json::Value,
        _sink: SideQuestionSink,
    ) -> Result<serde_json::Value, String> {
        Err("SideQuestion is not supported by this host; upgrade rpi".into())
    }

    /// Refresh the host-side model selector after provider packages reload.
    fn replace_model_catalog(&self, _models: Vec<rpi_ai::Model>) {}
    /// Invalidate child handles owned by an unloading/reloaded plugin.
    fn cancel_child_agents(&self, _owner: u64) {}
    /// `SendMessage` — Pi-compatible `triggerTurn` and `deliverAs` scheduling.
    /// Defaults append while idle and steer while running; explicit `true`
    /// starts an idle run, `false` only updates context, and `nextTurn` waits
    /// for a new explicit prompt. Results distinguish appended/queued/run.
    async fn send_message(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `SendUserMessage` — drive a run from a user-text message.
    async fn send_user_message(&self, args: serde_json::Value)
        -> Result<serde_json::Value, String>;
    /// `AppendEntry` — append without starting a run. `customType`/`data`
    /// persists extension data only. A `message` is durably queued during an
    /// operation and committed after its current turn; the returned `entryId`
    /// is reserved and may not yet resolve to a transcript entry. Optional
    /// `expected: {sessionId, leafId, customType, entryId}` performs a conditional
    /// write against that exact branch snapshot; a successful reply must confirm
    /// `scoped: true`. Scoped messages additionally require `outboxKey` and must
    /// be custom messages: acceptance is idempotent on the current branch and
    /// pending context stays with its source state entry.
    async fn append_entry(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `SetSessionName` — set the session's display name.
    async fn set_session_name(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `GetActiveTools` — the active tool-name list.
    async fn get_active_tools(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `SetActiveTools` — replace the active tool-name list.
    async fn set_active_tools(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `SetModel` — switch the active model (by id; host resolves to a `Model`).
    async fn set_model(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `GetThinkingLevel` — the current thinking level.
    async fn get_thinking_level(
        &self,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String>;
    /// `SetThinkingLevel` — set the thinking level.
    async fn set_thinking_level(
        &self,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String>;
    /// `Compact` — compact the session transcript.
    async fn compact(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `GetSystemPrompt` — the live composed system prompt.
    async fn get_system_prompt(&self, args: serde_json::Value)
        -> Result<serde_json::Value, String>;
    /// `NewSession` — start a fresh session and switch to it.
    async fn new_session(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `Fork` — fork the current session and switch to the fork.
    async fn fork(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `NavigateTree` — navigate/rewind the session tree.
    async fn navigate_tree(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `SwitchSession` — hot-switch to an existing session by id.
    async fn switch_session(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `Reload` — re-run extension discovery + Part-A resource loaders (a CLI
    /// concern; the host impl wires it to the `ActionBridge.reload` callback in
    /// B5d — until then this returns an "unsupported" error string).
    async fn reload(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;

    /// `ReloadModels` — re-read the model/provider catalog from disk **without**
    /// reloading extensions.
    ///
    /// The cheap counterpart of [`Self::reload`]: it swaps the model catalog
    /// and the provider runtime bindings, but leaves the extension registry,
    /// the emitter and the event snapshot untouched — so an in-flight run keeps
    /// receiving plugin events.
    ///
    /// Defaults to "not supported" so hosts that never wired it keep working.
    async fn reload_models(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        Err("reload_models is not supported by this host".to_string())
    }

    /// `GetCliFlag` — read a parsed extension CLI flag. Args are
    /// `{"name":"flag"}` and the result is `{"value": <bool|string|null>}`.
    async fn get_cli_flag(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        Err("CLI flag lookup is not configured".to_string())
    }

    /// `UiDialog` fallback for hosts that do not install an interactive
    /// mailbox. The default is deliberately an error so headless runs cannot
    /// accidentally report a fabricated user answer.
    async fn ui_dialog(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        Err("ask_user requires an interactive UI".to_string())
    }

    /// Enumerate the registered slash commands.
    ///
    /// Args: `{}`. Result: `{"commands":[{name, description, source}]}` where
    /// `source` is `builtin` / `extension` / `prompt`. A host with no registry
    /// returns an empty list (genuinely "none registered"), not an error: the
    /// remote UI should degrade to "no completion available".
    ///
    /// Added because the plugin ABI had no way to enumerate commands, so a
    /// remote client could not offer slash-command completion at all — it saw
    /// an empty list and could not tell that apart from "no host attached".
    async fn list_commands(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({ "commands": [] }))
    }

    /// Publish a freshly loaded extension registry to the host.
    ///
    /// Called after extension discovery **and after a successful `/reload`**, so
    /// `ListCommands` keeps answering with the current command set. Hosts that
    /// do not cache a registry (RPC mode reads the snapshot directly) keep the
    /// default no-op.
    ///
    /// Why it is on the trait instead of a downcast: `/reload` only has the
    /// `Arc<dyn RuntimeActionHost>` from the old bridge, so there is no concrete
    /// type to downcast to. A default no-op keeps every other host source-compatible.
    fn publish_registry(&self, _snapshot: std::sync::Arc<crate::registry::RegistrySnapshot>) {}

    /// Publish the registry, or `None` when a reload found no plugins at all.
    ///
    /// Distinct from [`Self::publish_registry`] because "no extensions" is a
    /// real state: a nonempty -> empty reload must replace the cached snapshot
    /// rather than leave removed commands advertised.
    fn publish_registry_opt(
        &self,
        snapshot: Option<std::sync::Arc<crate::registry::RegistrySnapshot>>,
    ) {
        if let Some(snapshot) = snapshot {
            self.publish_registry(snapshot);
        }
    }

    /// Abort the current run on the main lane.
    ///
    /// Args: `{}`. Result: `{"aborted": bool, "runId": "…"?, "steer": [...],
    /// "followUp": [...]}`. Reclaimed queue entries are `{"text","images"}`.
    ///
    /// The default implementation reports "nothing to cancel": a host with no
    /// run loop has nothing to abort, and a stop button must stay idempotent.
    async fn abort(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({
            "aborted": false,
            "steer": [],
            "followUp": [],
        }))
    }

    /// Finish the current turn, then stop — the gentle counterpart of
    /// [`Self::abort`].
    ///
    /// Args: `{}`; result: `{"requested": true}`. The flag is one-shot and
    /// cleared when the next run starts, so setting it while idle is harmless.
    ///
    /// The default implementation reports "requested" without acting: a host
    /// with no run loop has nothing to stop at a turn boundary, and returning an
    /// error would make a plugin treat a benign call as a failure.
    fn stop_round(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({ "requested": true }))
    }

    /// Safe, effective runtime metadata; never serialize credential bindings.
    async fn get_execution_context(
        &self,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        Err("execution context is not supported by this host".into())
    }

    /// Independent child lifecycle. `owner` is supplied by the bridge, NOT JSON.
    async fn child_agent(
        &self,
        _owner: u64,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        Err("ChildAgent is not supported by this host".into())
    }

    /// `GetSessionBranch` — query custom entries on the current main branch.
    /// Required: non-empty `customTypes` string array. Optional: `afterSeq`
    /// (exclusive), `limit` (positive), `order` (`oldestFirst` default or
    /// `newestFirst`). Unknown/legacy keys and unfiltered `{}` are rejected.
    /// Sorting/limit apply globally across the requested types; identifiers
    /// match exactly. Consumers must require `filtered == true` in the reply
    /// rather than accepting an older host's unfiltered successful response.
    /// This host also returns pinned `sessionId`, `leafId`, and `scopedAppend:
    /// true` for callers performing conditional state writes or outbox delivery.
    async fn get_session_branch(
        &self,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        Err("session branch lookup is not configured".to_string())
    }
}

/// The host-side bridge carried in [`PluginApi::user_data`] so
/// [`trampoline_runtime_action`] can recover the harness state from any thread.
///
/// `runtime: Handle` is captured at build time (the host is on the runtime when
/// it constructs the bridge) — this is the foreign-thread fix. `host` is the
/// `rpi-cli` impl over the harness. `reload` is a CLI-owned callback that
/// re-runs extension discovery + Part-A loaders (a CLI concern, NOT a harness
/// op) — wired by `rpi-cli` in B5d via [`reload_callback_from_mailbox`].
///
/// ## Staleness (B5d)
///
/// `active: Arc<AtomicBool>` is shared with a [`ReloadMailbox`]-driven swap
/// site. A `/reload` (TUI command OR a plugin's `runtime_action(Reload)`) builds
/// a fresh `ExtensionSession` + a fresh `ActionBridge`, calls
/// [`invalidate`](Self::invalidate) on the old bridge, and swaps the new one in.
/// In-flight `runtime_action` calls that recovered the OLD bridge from
/// `user_data` (the pointer a plugin stored during the prior `register`) then
/// hit the staleness guard in [`run_action`] and fail with a structured error
/// instead of driving a half-swapped harness. (Plugins load fresh on reload,
/// handing them the NEW bridge pointer; the guard only catches the race window
/// where an old call is still parked on `rx.recv()`.)
///
/// Held behind `Arc`. FFI callers carry only an opaque registry handle; each
/// dispatch upgrades its weak entry before accessing the bridge. A tool driver
/// owns its invocation bridge through its last foreign callback. Failed reloads
/// and dropped candidate sessions cannot leave dereferenceable dangling pointers.
pub struct ActionBridge {
    /// Some(child) only for a live host-created tool invocation.
    invocation: Option<(bool, String, tokio_util::sync::CancellationToken)>,
    execution_context: Option<serde_json::Value>,
    tool_program: Option<Arc<rpi_agent::tool_program::ToolProgram>>,
    stdio: crate::StdioLifecycle,
    plugin_identity: Option<(u64, String)>,
    component_owners: Arc<std::sync::Mutex<Vec<u64>>>,
    /// Captured at build time from a thread running the target runtime.
    pub(crate) runtime: Handle,
    /// The host impl (`HarnessActionHost` in rpi-cli).
    pub(crate) host: Arc<dyn RuntimeActionHost>,
    /// B5d reload callback; `None` until the TUI wires `/reload`.
    pub(crate) reload:
        Option<Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>>,
    /// Session-scoped UI mailbox. It is cloned into reload-created bridges so
    /// a TUI attachment and in-flight requests survive extension reloads.
    ui_dialog: UiDialogMailbox,
    /// Session-scoped extension status registry (`SetStatus` runtime action).
    /// Shared with the TUI, which polls [`ExtensionStatusMailbox::revision`]
    /// and repaints the footer only when an extension actually wrote.
    ext_status: crate::status::ExtensionStatusMailbox,
    /// Session-scoped editor-text queue (`SetEditorText` runtime action). Shared
    /// with the TUI, which drains it once per tick and writes the draft into the
    /// prompt editor.
    editor_text: crate::editor_text::EditorTextMailbox,
    /// B5d staleness flag. Shared so [`invalidate`] flips it for every clone.
    /// `true` while this bridge is the live session's bridge.
    active: Arc<AtomicBool>,
}

impl ActionBridge {
    /// Stable opaque context, valid only while this bridge is retained.
    pub fn user_data(self: &Arc<Self>) -> *mut c_void {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
        let mut registry = bridges().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((&id, _)) = registry
            .iter()
            .find(|(_, entry)| entry.bridge.as_ptr() == Arc::as_ptr(self))
        {
            return id as *mut c_void;
        }
        let mut id = NEXT.load(Ordering::Relaxed);
        loop {
            let next = id.checked_add(1).expect("runtime handle space exhausted");
            match NEXT.compare_exchange_weak(id, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(current) => id = current,
            }
        }
        registry.insert(
            id,
            RegistryEntry {
                bridge: Arc::downgrade(self),
                active: Arc::downgrade(&self.active),
            },
        );
        id as *mut c_void
    }
    pub(crate) fn resolve_handle(id: usize) -> Option<Arc<Self>> {
        bridges()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .and_then(|entry| entry.bridge.upgrade())
    }
    pub(crate) fn for_invocation(
        &self,
        child: bool,
        signal: tokio_util::sync::CancellationToken,
    ) -> Arc<Self> {
        let tool_program = rpi_agent::tool_program::current();
        if let Some(program) = &tool_program {
            program.bind_signal(signal.clone());
        }
        Arc::new(Self {
            invocation: Some((child, uuid::Uuid::new_v4().to_string(), signal)),
            execution_context: rpi_agent::tool_execution_scope::execution_context(),
            tool_program,
            stdio: self.stdio.clone(),
            plugin_identity: self.plugin_identity.clone(),
            component_owners: self.component_owners.clone(),
            runtime: self.runtime.clone(),
            host: self.host.clone(),
            reload: self.reload.clone(),
            ui_dialog: self.ui_dialog.clone(),
            ext_status: self.ext_status.clone(),
            editor_text: self.editor_text.clone(),
            active: self.active.clone(),
        })
    }
    /// Native receipt selection uses the driver-captured invocation, never a
    /// task-local lookup on a blocking worker or a registration/root binding.
    pub(crate) fn resolve_result_reference(
        &self,
        id: &str,
    ) -> Result<(rpi_agent::AgentToolResult, bool), String> {
        if !self.is_active()
            || !self
                .invocation
                .as_ref()
                .is_some_and(|(_, _, signal)| !signal.is_cancelled())
        {
            return Err("result reference requires a live invocation bridge".into());
        }
        self.tool_program
            .as_ref()
            .ok_or("result reference requires an effective ToolProgram")?
            .resolve_result_reference(id)
    }
    /// Content-only plans use the same captured live authority as full receipts.
    pub(crate) fn resolve_content_parts(
        &self,
        parts: Vec<rpi_agent::tool_program::ToolProgramContentPart>,
    ) -> Result<Vec<rpi_agent::TextContentOrImage>, String> {
        if !self.is_active()
            || !self
                .invocation
                .as_ref()
                .is_some_and(|(_, _, signal)| !signal.is_cancelled())
        {
            return Err("content parts require a live invocation bridge".into());
        }
        self.tool_program
            .as_ref()
            .ok_or("content parts require an effective ToolProgram")?
            .resolve_content_parts(parts)
    }
    pub(crate) fn invocation_key(&self) -> Option<&str> {
        self.invocation.as_ref().map(|(_, key, _)| key.as_str())
    }
    pub(crate) fn enter_invocation(self: &Arc<Self>) -> InvocationGuard {
        InvocationGuard {
            previous: CURRENT_INVOCATION.with(|slot| slot.replace(Some(self.clone()))),
            _thread_bound: std::marker::PhantomData,
        }
    }
    /// Build a bridge. The `Handle` MUST be captured from a thread running the
    /// target runtime (pi-cli builds the bridge on the async main thread).
    pub fn new(runtime: Handle, host: Arc<dyn RuntimeActionHost>) -> Arc<Self> {
        Arc::new(Self {
            invocation: None,
            tool_program: None,
            execution_context: None,
            stdio: crate::StdioLifecycle::default(),
            plugin_identity: None,
            component_owners: Arc::new(std::sync::Mutex::new(Vec::new())),
            runtime,
            host,
            reload: None,
            ui_dialog: UiDialogMailbox::new(),
            ext_status: crate::status::ExtensionStatusMailbox::new(),
            editor_text: crate::editor_text::EditorTextMailbox::new(),
            active: Arc::new(AtomicBool::new(true)),
        })
    }

    /// Same as [`new`](Self::new) with a reload callback (B5d wires this via
    /// [`reload_callback_from_mailbox`]).
    pub fn with_reload(
        runtime: Handle,
        host: Arc<dyn RuntimeActionHost>,
        reload: Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>,
    ) -> Arc<Self> {
        Self::with_reload_and_ui(
            runtime,
            host,
            reload,
            UiDialogMailbox::new(),
            crate::status::ExtensionStatusMailbox::new(),
            crate::editor_text::EditorTextMailbox::new(),
        )
    }

    /// Build a bridge with an explicit session-scoped UI mailbox.
    pub fn with_ui_dialog(
        runtime: Handle,
        host: Arc<dyn RuntimeActionHost>,
        ui_dialog: UiDialogMailbox,
    ) -> Arc<Self> {
        Arc::new(Self {
            invocation: None,
            tool_program: None,
            execution_context: None,
            stdio: crate::StdioLifecycle::default(),
            plugin_identity: None,
            component_owners: Arc::new(std::sync::Mutex::new(Vec::new())),
            runtime,
            host,
            reload: None,
            ui_dialog,
            ext_status: crate::status::ExtensionStatusMailbox::new(),
            editor_text: crate::editor_text::EditorTextMailbox::new(),
            active: Arc::new(AtomicBool::new(true)),
        })
    }

    /// `with_reload` variant that preserves a mailbox across bridge swaps.
    pub fn with_reload_and_ui(
        runtime: Handle,
        host: Arc<dyn RuntimeActionHost>,
        reload: Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>,
        ui_dialog: UiDialogMailbox,
        ext_status: crate::status::ExtensionStatusMailbox,
        editor_text: crate::editor_text::EditorTextMailbox,
    ) -> Arc<Self> {
        Arc::new(Self {
            invocation: None,
            tool_program: None,
            execution_context: None,
            stdio: crate::StdioLifecycle::default(),
            plugin_identity: None,
            component_owners: Arc::new(std::sync::Mutex::new(Vec::new())),
            runtime,
            host,
            reload: Some(reload),
            ui_dialog,
            ext_status,
            editor_text,
            active: Arc::new(AtomicBool::new(true)),
        })
    }

    pub(crate) fn for_plugin(&self, name: &str) -> Arc<Self> {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let owner = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        self.component_owners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(owner);
        Arc::new(Self {
            invocation: None,
            tool_program: None,
            execution_context: None,
            component_owners: self.component_owners.clone(),
            stdio: self.stdio.clone(),
            plugin_identity: Some((owner, name.to_owned())),
            runtime: self.runtime.clone(),
            host: self.host.clone(),
            reload: self.reload.clone(),
            ui_dialog: self.ui_dialog.clone(),
            ext_status: self.ext_status.clone(),
            editor_text: self.editor_text.clone(),
            active: self.active.clone(),
        })
    }

    pub fn stdio_lifecycle(&self) -> crate::StdioLifecycle {
        self.stdio.clone()
    }

    pub(crate) fn claim_stdio(&self) -> Result<(), String> {
        let (id, name) = self
            .plugin_identity
            .as_ref()
            .ok_or("missing plugin identity")?;
        self.stdio.claim(*id, name)
    }

    pub(crate) fn release_stdio_claim(&self) {
        if let Some((id, _)) = &self.plugin_identity {
            self.stdio.release_failed_registration(*id);
            self.ext_status.remove_owner(*id);
        }
    }

    /// Clone the session mailbox for TUI attachment or a reload-created bridge.
    pub fn ui_dialog_mailbox(&self) -> UiDialogMailbox {
        self.ui_dialog.clone()
    }

    /// Clone the session status registry (TUI rendering + reload-created bridges).
    pub fn extension_status_mailbox(&self) -> crate::status::ExtensionStatusMailbox {
        self.ext_status.clone()
    }

    /// Clone the session editor-text queue (TUI drain + reload-created bridges).
    pub fn editor_text_mailbox(&self) -> crate::editor_text::EditorTextMailbox {
        self.editor_text.clone()
    }

    /// Mark this bridge stale (B5d). A `/reload` that swaps in a fresh bridge
    /// calls this on the old one so in-flight `runtime_action` calls parked on
    /// the old `user_data` pointer fail fast with a staleness error instead of
    /// driving the swapped-out session. Idempotent.
    pub fn invalidate(&self) {
        let owners = {
            let owners = self
                .component_owners
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            self.active.store(false, Ordering::SeqCst);
            for owner in owners.iter() {
                self.ext_status.remove_owner(*owner);
            }
            owners.clone()
        };
        // Cancel captured capabilities across FFI thread hops on reload/session retirement.
        let live = self.bridges_for_owner();
        for bridge in live {
            if let Some(program) = &bridge.tool_program {
                program.cancel_all();
            }
        }
        // Host cancellation may contend with a job publishing through its
        // sink. Never invoke it while holding the sink's lifecycle gate.
        for owner in owners {
            self.host.cancel_child_agents(owner);
            self.host.cancel_side_questions(owner);
        }
    }

    fn bridges_for_owner(&self) -> Vec<Arc<Self>> {
        let active = Arc::downgrade(&self.active);
        // Filter weak owner identity BEFORE upgrading: even a transient upgrade
        // of another session's invocation delays its immediate handle retirement.
        // The weak metadata keeps that allocation's identity from being reused.
        // Collect every upgraded Arc out of the registry lock; dropping a last
        // Arc here would deadlock when ActionBridge::drop locks the registry.
        bridges()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|entry| entry.active.ptr_eq(&active))
            .filter_map(|entry| entry.bridge.upgrade())
            .collect()
    }

    /// Whether this bridge is still the live session's bridge.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// B5d: clone the host impl so a `/reload` can build a FRESH `ActionBridge`
    /// over the SAME `RuntimeActionHost` (the host's harness cell already points
    /// at the live harness — the harness is NOT rebuilt on reload — so the host
    /// is reusable across reloads; only the bridge's staleness flag + reload
    /// callback differ). The fresh bridge gets a fresh `active` flag (true) +
    /// the reload callback the TUI installed; the old bridge is `invalidate`d.
    pub fn clone_host(&self) -> Arc<dyn RuntimeActionHost> {
        Arc::clone(&self.host)
    }
}

/// A reload-signal mail slot (B5d). The reload callback (built by
/// [`reload_callback_from_mailbox`]) captures a clone; the TUI installs a
/// `tokio` unbounded sender after it starts. When a plugin calls
/// `runtime_action(Reload)`, the callback signals `()` (if a TUI is installed)
/// and the TUI performs the reload **asynchronously** — the plugin's call
/// returns `Ok(null)` immediately, so the calling plugin's cdylib is NOT
/// unmapped while its `runtime_action` frame is still on the stack (the reload,
/// which drops the old keepalive, happens after the call returns). This breaks
/// the self-unmapping race a synchronous plugin-initiated reload would have.
///
/// rpi-extensions carries only `()` (no pi-cli `TuiMessage` type) — preserving
/// the leaf DAG. The TUI owns the receiver + the actual reload routine.
#[derive(Clone)]
pub struct ReloadMailbox {
    tx: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<()>>>>,
}

impl Default for ReloadMailbox {
    fn default() -> Self {
        Self {
            tx: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

impl ReloadMailbox {
    /// A fresh empty mail slot (no TUI installed yet).
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the TUI's reload-signal sender (after the TUI starts). Replaces
    /// any prior sender. The TUI drops the receiver on shutdown; a sender held
    /// here keeps the channel half-open, so [`clear`] on shutdown is advised.
    pub fn install(&self, tx: tokio::sync::mpsc::UnboundedSender<()>) {
        *self.tx.lock().unwrap() = Some(tx);
    }

    /// Signal a reload (plugin-initiated via `runtime_action(Reload)`).
    /// `Ok(())` if a TUI is installed (the signal was enqueued; the TUI may
    /// still be mid-reload). `Err(())` if no TUI is installed (the host returns
    /// a "reload not available" error to the plugin).
    pub fn signal(&self) -> Result<(), ()> {
        let g = self.tx.lock().unwrap();
        match &*g {
            Some(tx) => {
                let _ = tx.send(());
                Ok(())
            }
            None => Err(()),
        }
    }

    /// Drop the installed sender (TUI shutdown). Idempotent.
    pub fn clear(&self) {
        *self.tx.lock().unwrap() = None;
    }
}

/// A request/reply mailbox for `runtime_action(ReloadModels)`.
///
/// Unlike [`ReloadMailbox`] (fire-and-forget: the plugin does not care whether
/// the reload succeeded), this one **waits for the result**: the caller wants
/// to know how many models the refreshed catalog holds, and whether the
/// re-read failed at all.
///
/// The TUI owns the receiver and performs the actual work (it owns the
/// harness); the plugin-facing side only enqueues a request and awaits the
/// reply.
pub struct ReloadModelsMailbox {
    tx: Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<ReloadModelsRequest>>>>,
}

impl Default for ReloadModelsMailbox {
    fn default() -> Self {
        Self {
            tx: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

/// One pending `ReloadModels` request plus the channel to answer on.
pub struct ReloadModelsRequest {
    /// Answer channel: `Ok(model_count)` or `Err(reason)`.
    pub reply: tokio::sync::oneshot::Sender<Result<usize, String>>,
}

impl Clone for ReloadModelsMailbox {
    fn clone(&self) -> Self {
        Self {
            tx: Arc::clone(&self.tx),
        }
    }
}

impl ReloadModelsMailbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the TUI's request sender. Replaces any prior sender.
    pub fn install(&self, tx: tokio::sync::mpsc::UnboundedSender<ReloadModelsRequest>) {
        *self.tx.lock().unwrap() = Some(tx);
    }

    /// Enqueue a request and await the TUI's answer.
    ///
    /// `Err` when no TUI is installed (the host surfaces "not available") or
    /// when the TUI went away before answering.
    pub async fn request(&self) -> Result<usize, String> {
        let (reply, wait) = tokio::sync::oneshot::channel();
        let tx = self.tx.lock().unwrap().clone();
        let Some(tx) = tx else {
            return Err("reload_models is not available (no TUI attached)".to_string());
        };
        tx.send(ReloadModelsRequest { reply })
            .map_err(|_| "reload_models request dropped (TUI gone)".to_string())?;
        wait.await
            .map_err(|_| "reload_models request dropped (TUI gone)".to_string())?
    }

    /// Drop the installed sender (TUI shutdown). Idempotent.
    pub fn clear(&self) {
        *self.tx.lock().unwrap() = None;
    }
}

/// Build the reload callback the bridge carries, backed by a [`ReloadMailbox`].
/// When a plugin calls `runtime_action(Reload)`, the bridge's spawn site awaits
/// this callback, which signals the TUI (if installed) and returns; the plugin
/// receives `Ok(null)` and the TUI performs the reload asynchronously. If no
/// TUI is installed, the callback returns without signalling and the host's
/// [`RuntimeActionHost::reload`] fallback surfaces the "not configured" error.
pub fn reload_callback_from_mailbox(
    mailbox: ReloadMailbox,
) -> Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync> {
    Arc::new(move || {
        let m = mailbox.clone();
        Box::pin(async move {
            let _ = m.signal();
        })
    })
}

// `Handle` is Send+Sync, `Arc<dyn RuntimeActionHost>` (with `Send + Sync` bound)
// is Send+Sync, and the reload closure is `Send + Sync` — so `ActionBridge` is
// naturally Send+Sync; no manual unsafe impl needed.

/// Dispatch one action to the host. Async — runs on the bridge's runtime.
/// Handles all 19 ids; `Reload` delegates to [`RuntimeActionHost::reload`]
/// (the "no callback configured" fallback). When the bridge has a reload
/// callback, the spawn site intercepts `Reload` and awaits the callback
/// instead (a CLI concern, not a harness op) — this helper is the plain
/// host-only path.
/// The mailbox-only `RelayUiDialog` action.
///
/// Split out so the trampoline can run it on a plain thread instead of a Tokio
/// worker (see the `RelayUiDialog` arm at the spawn site).
fn dispatch_relay_ui_dialog(
    ui_dialog: &UiDialogMailbox,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let op = args
        .get("op")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("take");
    match op {
        "take" => {
            // Taking implies ownership: the caller is the UI that will present
            // these questions, so the local TUI must stand down rather than
            // compete for the same queue.
            ui_dialog.set_external_takeover(true);
            let requests: Vec<serde_json::Value> = ui_dialog
                .take_all_unreserved()
                .into_iter()
                .map(|request| {
                    serde_json::json!({
                        "requestId": request.request_id,
                        "toolCallId": request.tool_call_id,
                        "ui": request.ui,
                    })
                })
                .collect();
            Ok(serde_json::json!({ "requests": requests }))
        }
        // Give delivery back to the local TUI and re-queue anything the relay
        // took but never answered.
        "release" => {
            let requeued = ui_dialog.release_takeover();
            Ok(serde_json::json!({ "requeued": requeued }))
        }
        // Same request ids, same semantics as `UiDialog`.
        "respond" | "cancel" => ui_dialog.handle(args),
        other => Err(format!("RelayUiDialog does not support op `{other}`")),
    }
}

async fn dispatch(
    host: &Arc<dyn RuntimeActionHost>,
    ui_dialog: &UiDialogMailbox,
    ext_status: &crate::status::ExtensionStatusMailbox,
    editor_text: &crate::editor_text::EditorTextMailbox,
    action: RuntimeActionId,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    match action {
        RuntimeActionId::SendMessage => host.send_message(args).await,
        RuntimeActionId::SendUserMessage => host.send_user_message(args).await,
        RuntimeActionId::AppendEntry => host.append_entry(args).await,
        RuntimeActionId::SetSessionName => host.set_session_name(args).await,
        RuntimeActionId::GetActiveTools => host.get_active_tools(args).await,
        RuntimeActionId::SetActiveTools => host.set_active_tools(args).await,
        RuntimeActionId::SetModel => host.set_model(args).await,
        RuntimeActionId::GetThinkingLevel => host.get_thinking_level(args).await,
        RuntimeActionId::SetThinkingLevel => host.set_thinking_level(args).await,
        RuntimeActionId::Compact => host.compact(args).await,
        RuntimeActionId::GetSystemPrompt => host.get_system_prompt(args).await,
        RuntimeActionId::NewSession => host.new_session(args).await,
        RuntimeActionId::Fork => host.fork(args).await,
        RuntimeActionId::NavigateTree => host.navigate_tree(args).await,
        RuntimeActionId::SwitchSession => host.switch_session(args).await,
        RuntimeActionId::Reload => host.reload(args).await,
        RuntimeActionId::ReloadModels => host.reload_models(args).await,
        RuntimeActionId::GetCliFlag => host.get_cli_flag(args).await,
        RuntimeActionId::UiDialog => ui_dialog.handle(args),
        // UI-only action: handled by the bridge (no harness call), exactly like
        // `UiDialog`. Written straight into the shared registry the TUI polls.
        RuntimeActionId::SetStatus => ext_status.handle(args),
        RuntimeActionId::UiComponent => Err("UiComponent requires a plugin bridge".into()),
        // Editor-only action: served by the bridge (no harness call), exactly
        // like `SetStatus`/`UiDialog`. The TUI drains the queue each tick.
        RuntimeActionId::SetEditorText => editor_text.handle(args),
        RuntimeActionId::GetSessionBranch => host.get_session_branch(args).await,
        RuntimeActionId::GetExecutionContext => host.get_execution_context(args).await,
        RuntimeActionId::ToolProgram => Err("ToolProgram requires a live invocation bridge".into()),
        RuntimeActionId::ChildAgent => Err("ChildAgent requires a plugin bridge".into()),
        RuntimeActionId::SideQuestion => Err("SideQuestion requires a plugin bridge".into()),
        RuntimeActionId::ListCommands => host.list_commands(args).await,
        RuntimeActionId::Abort => host.abort(args).await,
        // Hand the mailbox to an external UI. Only correct while one is
        // attached — with no client, the caller should let the TUI draw the
        // dialog instead (otherwise nobody sees the question).
        RuntimeActionId::RelayUiDialog => dispatch_relay_ui_dialog(ui_dialog, args),
        RuntimeActionId::StopRound => host.stop_round(args),
        RuntimeActionId::StdioLifecycle => {
            Err("stdio lifecycle requires a plugin context".to_owned())
        }
    }
}

/// The real `runtime_action` trampoline — replaces `stub_runtime_action` when a
/// bridge is present (see [`HostApi::build_vtable`](crate::HostApi)).
///
/// Resolves an `Arc<ActionBridge>` from an opaque handle (or the active tool
/// invocation), parses `args_json`, drives the host's async dispatch via
/// `Handle::spawn`, and parks
/// the plugin thread on a std `mpsc` `recv` (works from ANY thread — no ambient
/// runtime needed, which is the load-bearing property for foreign plugin
/// threads).
///
/// ## Return codes
/// - `0` — success; `*out` written with the result JSON (host-owned
///   [`StbString`]; the plugin frees it via the host `free_string` from the
///   vtable).
/// - `1` — host-level error; `*out` written with `{"error": msg}` JSON.
/// - `2` — unknown numeric action id; `*out` contains a structured error and
///   no host method is dispatched.
/// - `-1` — no bridge present (`user_data` null; should not happen when wired).
/// - `-2` — the spawned task dropped its sender without sending (runtime
///   shutdown / dispatch panic); no result available.
///
/// The whole body is `catch_unwind`-wrapped — a panic across FFI ⇒ abort (same
/// policy as the tool partial callback, `tool.rs:206`).
// The host ABI passes `out` as a raw pointer; the trampoline writes through it
// under the same external-ABI contract as the surrounding plugin API.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn trampoline_runtime_action(
    action_id: u32,
    args_json: StbStringRef,
    out: *mut StbString,
    user_data: *mut c_void,
) -> i32 {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_action(
            action_id,
            // Highest id this ABI defines.
            rpi_plugin_sdk::RuntimeActionId::MAX,
            args_json,
            out,
            user_data,
        )
    }));
    match outcome {
        Ok(rc) => rc,
        Err(_) => {
            // A panic in the host-side dispatch was caught. Do not abort the
            // process: report a host-level error to the caller (documented
            // code `1`) with a structured payload when `out` is writable.
            tracing::error!("runtime_action trampoline panicked — reporting host error");
            if !out.is_null() {
                unsafe {
                    *out = StbString::from_string(
                        r#"{"error":"runtime_action panicked"}"#.to_string(),
                    );
                }
            }
            1
        }
    }
}

/// Inner synchronous driver (split out so the `catch_unwind` wrapper is clean).
fn run_action(
    action_id: u32,
    max_action_id: u32,
    args_json: StbStringRef,
    out: *mut StbString,
    user_data: *mut c_void,
) -> i32 {
    // Validate the raw FFI integer before dereferencing host state or entering
    // the enum-based dispatch. Unknown values are ordinary protocol errors;
    // they never become invalid Rust enum discriminants.
    let action = match RuntimeActionId::try_from(action_id) {
        Ok(action) if action_id <= max_action_id => action,
        Ok(_) | Err(_) => {
            if !out.is_null() {
                let json = serde_json::json!({
                    "error": format!("unknown runtime action id {action_id}")
                })
                .to_string();
                // SAFETY: `out` is non-null and points to the caller-provided
                // output slot. The plugin reclaims this host allocation via
                // `host_free_string` from the vtable.
                unsafe {
                    *out = StbString::from_string(json);
                }
            }
            return 2;
        }
    };

    if user_data.is_null() {
        return -1;
    }
    // Synchronous callbacks inherit the actual driver's invocation, not a
    // plugin-global registration pointer. Worker threads pass its opaque ID.
    let invocation = CURRENT_INVOCATION.with(|slot| slot.borrow().clone());
    let invocation_thread = invocation.is_some();
    let bridge = invocation.or_else(|| ActionBridge::resolve_handle(user_data as usize));
    let Some(bridge) = bridge else {
        if !out.is_null() {
            // SAFETY: the ABI caller supplies a writable output slot.
            unsafe {
                *out = StbString::from_string("{\"error\":\"expired runtime handle\"}".into());
            }
        }
        return 1;
    };

    // B5d staleness guard: a `/reload` that swapped in a fresh bridge calls
    // `invalidate` on the old one. A plugin that still holds the old pointer
    // (stored during the prior `register`) must not drive the swapped-out
    // session. Surface a structured "stale bridge" error so the plugin's
    // `runtime_action` returns nonzero + `{"error": ...}` instead of racing
    // the swap. (The new bridge's pointer was handed to the reloaded plugins;
    // this guard only catches the race window where an old call is still parked.)
    if !bridge.is_active() {
        if out.is_null() {
            return 1;
        }
        let json = serde_json::json!({
            "error": "runtime_action on a stale ActionBridge (session reloaded/swapped)"
        })
        .to_string();
        unsafe {
            *out = StbString::from_string(json);
        }
        return 1;
    }

    // Parse args. An empty/invalid JSON blob collapses to `{}` — getters ignore
    // args; setters that require a field surface a clear error string.
    let args_str = unsafe { args_json.as_str() };
    let mut args: serde_json::Value = if args_str.is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(args_str)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()))
    };

    let runtime_key = args
        .as_object_mut()
        .and_then(|args| args.remove("__rpiRuntimeKey"));

    // Drive the host's async dispatch on the bridge's runtime. STD mpsc so the
    // plugin thread can recv from any thread (no ambient runtime). sync_channel
    // (1): bounded; the send completes once the value is delivered. rx.recv()
    // parks the plugin thread until the spawn finishes (or its sender drops).
    let (tx, rx) = mpsc::sync_channel::<Result<serde_json::Value, String>>(1);
    // Clone the bridge's host + reload callback into the spawned task. We pass
    // an owned `Arc<dyn RuntimeActionHost>` plus a reload-option snapshot so the
    // `dispatch` helper has everything it needs without borrowing `bridge`.
    let host = Arc::clone(&bridge.host);
    let ui_dialog = bridge.ui_dialog.clone();
    let ext_status = bridge.ext_status.clone();
    let editor_text = bridge.editor_text.clone();
    let reload_cb = bridge.reload.clone();
    let execution_context = bridge.execution_context.clone();
    if action == RuntimeActionId::StdioLifecycle {
        let result = bridge.stdio.handle(
            bridge.plugin_identity.as_ref().map_or(0, |(id, _)| *id),
            args,
        );
        let _ = tx.send(result);
    } else if action == RuntimeActionId::UiComponent {
        let owner = bridge.plugin_identity.as_ref().map_or(0, |(id, _)| *id);
        let _owners = bridge
            .component_owners
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let result = if bridge.is_active() {
            ext_status.component(owner, args)
        } else {
            Err("stale plugin bridge".into())
        };
        let _ = tx.send(result);
    } else if action == RuntimeActionId::SideQuestion {
        let owner = bridge.plugin_identity.as_ref().map_or(0, |(id, _)| *id);
        if owner == 0 || bridge.invocation.is_some() {
            let _ = tx.send(Err(
                "SideQuestion requires a plugin command, not a tool invocation".into(),
            ));
        } else {
            // Serialize writes with invalidate(): no late publication can
            // resurrect components after owner retirement. Keep only weak state.
            let weak = Arc::downgrade(&bridge);
            let sink: SideQuestionSink = Arc::new(move |args| {
                let bridge = weak.upgrade().ok_or("expired side question bridge")?;
                let _owners = bridge
                    .component_owners
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if !bridge.is_active() {
                    return Err("stale side question bridge".into());
                }
                if args["op"] == "check" {
                    return Ok(serde_json::json!({"ok":true}));
                }
                bridge.ext_status.component(owner, args)
            });
            bridge.runtime.spawn(async move {
                let _ = tx.send(host.side_question(owner, args, sink).await);
            });
        }
    } else if action == RuntimeActionId::GetActiveTools && bridge.tool_program.is_some() {
        let tools = bridge
            .tool_program
            .as_ref()
            .expect("checked")
            .active_tool_names();
        let _ = tx.send(Ok(serde_json::json!({"tools":tools})));
    } else if action == RuntimeActionId::SetActiveTools && bridge.execution_context.is_some() {
        let _ = tx.send(Err(
            "cannot replace active tools from a narrowed execution".into()
        ));
    } else if action == RuntimeActionId::ToolProgram {
        let cleanup = matches!(
            args.get("op").and_then(serde_json::Value::as_str),
            Some("poll" | "cancel" | "dispose" | "release_result")
        );
        let authenticated = bridge.invocation.as_ref().is_some_and(|(_, key, signal)| {
            (cleanup || !signal.is_cancelled())
                && (invocation_thread
                    || runtime_key.as_ref().and_then(serde_json::Value::as_str)
                        == Some(key.as_str()))
        });
        if let Some(capability) = bridge.tool_program.clone().filter(|_| authenticated) {
            let signal = bridge.invocation.as_ref().expect("authenticated").2.clone();
            bridge.runtime.spawn(async move {
                if signal.is_cancelled() {
                    capability.cancel_all();
                    if !cleanup {
                        let _ = tx.send(Err("parent invocation is no longer active".into()));
                        return;
                    }
                }
                let _ = tx.send(host.tool_program(capability, args).await);
            });
        } else {
            let _ = tx.send(Err(
                "ToolProgram requires an authenticated executing invocation".into(),
            ));
        }
    } else if action == RuntimeActionId::ChildAgent {
        // A registration handle cannot assert that it is a parent invocation.
        // All child-originated invocations fail closed, regardless of tool name.
        // Cancellation stops admission, not authenticated reclamation. The
        // plugin's cancel/destroy path must still dispose its retained handles.
        let cleanup = matches!(
            args.get("op").and_then(serde_json::Value::as_str),
            Some("poll" | "cancel" | "dispose")
        );
        let allowed = bridge
            .invocation
            .as_ref()
            .is_some_and(|(child, key, signal)| {
                !child
                    && (cleanup || !signal.is_cancelled())
                    && (invocation_thread
                        || runtime_key.as_ref().and_then(serde_json::Value::as_str)
                            == Some(key.as_str()))
            });
        if !allowed {
            let _ = tx.send(Err("ChildAgent requires a live parent tool invocation; recursive child agents are not allowed".into()));
        } else {
            let owner = bridge.plugin_identity.as_ref().map_or(0, |(id, _)| *id);
            let signal = bridge
                .invocation
                .as_ref()
                .expect("authorized invocation")
                .2
                .clone();
            bridge.runtime.spawn(async move {
                let result = rpi_agent::tool_execution_scope::with_invocation_signal(
                    signal,
                    host.child_agent(owner, args),
                )
                .await;
                let _ = tx.send(result);
            });
        }
    } else if action == RuntimeActionId::SetStatus {
        // Same reasoning as `UiDialog`: this action never touches the harness,
        // so it must not need a Tokio worker. A plugin calling it from inside a
        // single-threaded runtime (a tool poll callback, say) would otherwise
        // park that executor and deadlock.
        std::thread::spawn(move || {
            let _ = tx.send(ext_status.handle(args));
        });
    } else if action == RuntimeActionId::SetEditorText {
        // Editor-only, like `SetStatus`: no harness call, so keep it off the
        // Tokio worker for the same single-thread-runtime reason.
        std::thread::spawn(move || {
            let _ = tx.send(editor_text.handle(args));
        });
    } else if action == RuntimeActionId::RelayUiDialog {
        // Mailbox-only, like `UiDialog`/`SetStatus`/`SetEditorText`: it never
        // touches the harness. Routing it through the generic runtime spawn
        // would park the blocking `rx.recv()` on a Tokio worker that the
        // mailbox-only op itself needs to complete — on a current-thread bridge
        // that deadlocks the only thread able to finish it.
        std::thread::spawn(move || {
            let _ = tx.send(dispatch_relay_ui_dialog(&ui_dialog, args));
        });
    } else if action == RuntimeActionId::UiDialog {
        // UiDialog is intentionally synchronous at the plugin ABI boundary:
        // `open` waits until the TUI answers. Never park that wait inside the
        // Tokio worker that services the bridge. A SEARCH/extension tool that
        // invokes `ui_request/open` from a single-thread runtime would
        // otherwise block the executor before the TUI can deliver the answer.
        std::thread::spawn(move || {
            let _ = tx.send(ui_dialog.handle(args));
        });
    } else {
        bridge.runtime.spawn(async move {
            let mut r = if action == RuntimeActionId::Reload {
                if let Some(cb) = reload_cb {
                    cb().await;
                    Ok(serde_json::Value::Null)
                } else {
                    host.reload(args).await
                }
            } else {
                dispatch(&host, &ui_dialog, &ext_status, &editor_text, action, args).await
            };
            if action == RuntimeActionId::GetExecutionContext {
                if let (Ok(context), Some(serde_json::Value::Object(fields))) =
                    (&mut r, execution_context)
                {
                    if let Some(object) = context.as_object_mut() {
                        object.extend(fields);
                    }
                }
            }
            // If the plugin thread already moved on (dropped rx), discard — a
            // send error is NOT a host fault.
            let _ = tx.send(r);
        });
    }

    // Event observers can call back synchronously from a Tokio worker. A task
    // just spawned above may still occupy that worker's non-stealable LIFO
    // slot. Yield the scheduler before blocking so dispatch can actually run.
    // Foreign/plugin workers and current-thread mailbox-only calls keep the
    // ordinary blocking receive.
    let received = if Handle::try_current()
        .is_ok_and(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
    {
        tokio::task::block_in_place(|| rx.recv())
    } else {
        rx.recv()
    };
    let result = match received {
        Ok(r) => r,
        Err(_) => {
            // Spawned task dropped the sender without sending: runtime shutdown
            // or the dispatch future panicked (caught inside dispatch? no —
            // dispatch is plain async, a panic would propagate to the spawn and
            // drop the sender). No result to return.
            return -2;
        }
    };

    // Write the result (or error) into `*out` as a host-owned StbString. The
    // plugin frees it via the vtable's `free_string` (= `host_free_string`).
    if out.is_null() {
        // Nothing to write to; still report the outcome via the return code.
        return match result {
            Ok(_) => 0,
            Err(_) => 1,
        };
    }

    let (rc, payload) = match result {
        Ok(value) => {
            let json = serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string());
            (0, json)
        }
        Err(msg) => {
            let json = serde_json::json!({ "error": msg }).to_string();
            (1, json)
        }
    };
    // SAFETY: `out` is a valid `*mut StbString` the plugin provided for this
    // call (null checked above). `from_string` allocates a `Box<[u8]>` the host
    // owns; the plugin reclaims it via `host_free_string` (which reconstructs
    // the Box from ptr+len — matches `from_string`'s allocation, same pattern
    // used by translate.rs/tool.rs).
    unsafe {
        *out = StbString::from_string(payload);
    }
    rc
}

// ===========================================================================
// Tests — round-trip the real trampoline + dispatch + Handle::spawn + mpsc
// against a mock host. This is the B5a unit proof: plugin→host `runtime_action`
// recovers the `ActionBridge` via `user_data`, drives the host's async method on
// the runtime, parks the caller on `rx.recv()`, and writes the result JSON back
// through `*out` (freed via `host_free_string`). No cdylib needed — the trampoline
// is the same `extern "C" fn` a plugin's vtable carries.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[tokio::test(flavor = "current_thread")]
    async fn stdio_claim_is_exclusive_and_only_its_scoped_context_can_finish() {
        let master = ActionBridge::new(
            Handle::current(),
            Arc::new(MockHost {
                prompt: String::new(),
                saw: Mutex::new(Vec::new()),
            }),
        );
        let diag: Arc<dyn crate::PluginDiagnostics> = Arc::new(crate::NullDiagnostics);
        let owner = crate::HostApi::with_action_bridge(
            "owner",
            crate::ExtensionRegistry::new(),
            diag.clone(),
            master.clone(),
        );
        let other = crate::HostApi::with_action_bridge(
            "other",
            crate::ExtensionRegistry::new(),
            diag,
            master.clone(),
        );
        let vtable = owner.build_vtable();
        let claim = r#"{"ownsStdio":true}"#;
        {
            let _registration = crate::enter_current_api(&owner);
            assert_eq!(vtable.declare.unwrap()(StbStringRef::from_str(claim)), 0);
        }
        let other_vtable = other.build_vtable();
        let finish = StbStringRef::from_str(r#"{"op":"finish","exitCode":7}"#);
        let mut out = StbString::empty();
        // Direct, on a single-thread executor: this action must not park on Tokio.
        assert_eq!(
            (other_vtable.runtime_action)(
                RuntimeActionId::StdioLifecycle as u32,
                finish,
                &mut out,
                other_vtable.user_data
            ),
            1
        );
        // SAFETY: the action returned a live allocation from this vtable.
        unsafe { (other_vtable.free_string)(out) };
        assert_eq!(
            (vtable.runtime_action)(
                RuntimeActionId::StdioLifecycle as u32,
                finish,
                &mut out,
                vtable.user_data
            ),
            0
        );
        // SAFETY: the action returned a live allocation from this vtable.
        unsafe { (vtable.free_string)(out) };
        assert_eq!(master.stdio_lifecycle().wait().await, 7);
        {
            let _registration = crate::enter_current_api(&other);
            assert_ne!(
                other_vtable.declare.unwrap()(StbStringRef::from_str(claim)),
                0
            );
        }
        assert!(master
            .stdio_lifecycle()
            .validate()
            .unwrap_err()
            .contains("owner"));
    }

    /// A minimal `RuntimeActionHost` that answers `get_system_prompt` with a
    /// canned string and `reload` with Ok(null); every other action returns an
    /// "unimplemented" error. Enough to prove the dispatch switch + the async
    /// spawn + the mpsc round-trip + the StbString write.
    struct MockHost {
        prompt: String,
        saw: Mutex<Vec<RuntimeActionId>>,
    }

    #[test]
    fn runtime_action_from_a_tokio_worker_does_not_block_its_dispatch() {
        const CHILD: &str = "RPI_RUNTIME_ACTION_WORKER_REGRESSION_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                // Lifecycle observers run directly on a worker. Their runtime
                // action spawns back onto that runtime, including its LIFO slot.
                tokio::spawn(async {
                    let host = Arc::new(MockHost {
                        prompt: "worker callback".into(),
                        saw: Mutex::new(Vec::new()),
                    });
                    let bridge = ActionBridge::new(Handle::current(), host);
                    let mut out = StbString::empty();
                    assert_eq!(
                        trampoline_runtime_action(
                            RuntimeActionId::GetSystemPrompt.into(),
                            StbStringRef::from_str("{}"),
                            &mut out,
                            bridge.user_data(),
                        ),
                        0
                    );
                    // SAFETY: the action returned a live host-owned string.
                    let text = unsafe {
                        rpi_plugin_sdk::callbacks::OwnedString::from_raw(
                            out,
                            Some(crate::host_free_string),
                        )
                    }
                    .to_string_lossy();
                    assert_eq!(
                        serde_json::from_str::<serde_json::Value>(&text).unwrap()["prompt"],
                        "worker callback"
                    );
                })
                .await
                .unwrap();
            });
            return;
        }
        // Isolate the old self-deadlock so a regression fails promptly instead
        // of hanging the test runner or leaving a blocked runtime thread behind.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "actions::tests::runtime_action_from_a_tokio_worker_does_not_block_its_dispatch",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("runtime action blocked the worker needed by its own dispatch");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[async_trait::async_trait]
    impl RuntimeActionHost for MockHost {
        async fn send_message(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn send_user_message(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn append_entry(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn set_session_name(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn get_active_tools(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn set_active_tools(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn set_model(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn get_thinking_level(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn set_thinking_level(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn compact(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn get_system_prompt(
            &self,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            self.saw
                .lock()
                .unwrap()
                .push(RuntimeActionId::GetSystemPrompt);
            Ok(serde_json::json!({ "prompt": self.prompt }))
        }
        async fn new_session(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn fork(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn navigate_tree(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn switch_session(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            unreachable!("not under test")
        }
        async fn reload(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            self.saw.lock().unwrap().push(RuntimeActionId::Reload);
            Ok(serde_json::Value::Null)
        }
        async fn get_cli_flag(&self, _: serde_json::Value) -> Result<serde_json::Value, String> {
            self.saw.lock().unwrap().push(RuntimeActionId::GetCliFlag);
            Ok(serde_json::json!({ "value": true }))
        }
    }

    /// Async host actions need a multi-thread runtime when called synchronously
    /// from inside it. The trampoline yields that worker with block_in_place;
    /// foreign/plugin threads instead park on the ordinary channel receive.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trampoline_round_trips_get_system_prompt() {
        let host = Arc::new(MockHost {
            prompt: "hello from host".to_string(),
            saw: Mutex::new(Vec::new()),
        });
        let host_for_assert = Arc::clone(&host);
        let host_dyn: Arc<dyn RuntimeActionHost> = host;
        let runtime = tokio::runtime::Handle::current();
        let bridge = ActionBridge::new(runtime, host_dyn);
        let user_data = bridge.user_data();

        // Build a StbStringRef for the args. `{}` — getters ignore it.
        let args_str = "{}";
        let args_ref = StbStringRef::from_str(args_str);

        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            RuntimeActionId::GetSystemPrompt.into(),
            args_ref,
            &mut out as *mut StbString,
            user_data,
        );
        assert_eq!(rc, 0, "success return code");

        // Read the result JSON back + reclaim the host-owned StbString.
        // SAFETY: the action wrote a live host-owned output, freed below.
        let json_text = unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
        }
        .to_string_lossy();
        let parsed: serde_json::Value = serde_json::from_str(&json_text).expect("valid json");
        assert_eq!(parsed["prompt"], "hello from host");

        // The host saw exactly the one action. `host_for_assert` is a clone of
        // the `Arc<MockHost>` kept before it was coerced to the trait object.
        let saw = host_for_assert.saw.lock().unwrap().clone();
        assert_eq!(saw, vec![RuntimeActionId::GetSystemPrompt]);
    }

    /// The `SetEditorText` runtime action must reach the bridge's editor-text
    /// mailbox through the ABI trampoline. This is the wiring a plugin relies on
    /// (a voice extension writes a transcription draft here); a broken action id
    /// or a dropped dispatch would be invisible to the plugin — it only sees
    /// `ok: true` — so pin it here.
    #[tokio::test]
    async fn trampoline_routes_set_editor_text_into_the_mailbox() {
        let host = Arc::new(MockHost {
            prompt: String::new(),
            saw: Mutex::new(Vec::new()),
        });
        let host_dyn: Arc<dyn RuntimeActionHost> = host;
        let bridge = ActionBridge::new(tokio::runtime::Handle::current(), host_dyn);
        let mailbox = bridge.editor_text_mailbox();
        let user_data = bridge.user_data();

        let args_ref =
            StbStringRef::from_str(r#"{"text":"from voice","mode":"append","autoSendMs":2000}"#);
        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            RuntimeActionId::SetEditorText.into(),
            args_ref,
            &mut out as *mut StbString,
            user_data,
        );
        assert_eq!(rc, 0, "success return code");

        // SAFETY: the action wrote a live host-owned output, freed below.
        let parsed: serde_json::Value = serde_json::from_str(
            &unsafe {
                rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
            }
            .to_string_lossy(),
        )
        .expect("valid json");
        assert_eq!(parsed["ok"], serde_json::json!(true));
        assert_eq!(parsed["chars"], serde_json::json!(10));

        // The queued edit is what the host will apply to the prompt editor.
        let edit = mailbox.take_pending().expect("queued edit");
        assert_eq!(edit.text, "from voice");
        assert_eq!(edit.mode, crate::EditorTextMode::Append);
        assert_eq!(edit.auto_send_ms, Some(2000));
        assert!(mailbox.take_pending().is_none());
    }

    #[tokio::test]
    async fn trampoline_null_user_data_returns_minus_one() {
        let args_ref = StbStringRef::from_str("{}");
        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            RuntimeActionId::GetSystemPrompt.into(),
            args_ref,
            &mut out as *mut StbString,
            std::ptr::null_mut(),
        );
        assert_eq!(rc, -1, "null user_data ⇒ no bridge");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trampoline_intercepts_reload_when_bridge_has_callback() {
        // A bridge with a reload callback: the spawn site intercepts Reload and
        // runs the callback instead of the host's `reload` method. Proves the
        // bridge-level special-casing (the host impl is the fallback only).
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reload_calls = Arc::new(AtomicUsize::new(0));
        let reload_calls_for_cb = Arc::clone(&reload_calls);
        let reload: Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync> =
            Arc::new(move || {
                let c = Arc::clone(&reload_calls_for_cb);
                Box::pin(async move {
                    c.fetch_add(1, Ordering::SeqCst);
                })
            });
        let host = Arc::new(MockHost {
            prompt: String::new(),
            saw: Mutex::new(Vec::new()),
        });
        let host_for_assert = Arc::clone(&host);
        let host_dyn: Arc<dyn RuntimeActionHost> = host;
        let runtime = tokio::runtime::Handle::current();
        let bridge = ActionBridge::with_reload(runtime, host_dyn, reload);
        let user_data = bridge.user_data();

        let args_ref = StbStringRef::from_str("{}");
        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            RuntimeActionId::Reload.into(),
            args_ref,
            &mut out as *mut StbString,
            user_data,
        );
        assert_eq!(rc, 0);
        // The callback ran once; the host's `reload` did NOT.
        assert_eq!(reload_calls.load(Ordering::SeqCst), 1);
        assert!(host_for_assert.saw.lock().unwrap().is_empty());
        // SAFETY: this action returned a live host allocation, not previously freed.
        drop(unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
        });
    }

    /// B5d: an invalidated bridge rejects `runtime_action` with a stale-bridge
    /// error instead of dispatching. A `/reload` calls `invalidate` on the old
    /// bridge; an in-flight call that still holds the old pointer must fail
    /// fast rather than drive the swapped-out session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trampoline_rejects_stale_bridge_with_invalidate() {
        let host = Arc::new(MockHost {
            prompt: String::new(),
            saw: Mutex::new(Vec::new()),
        });
        let host_for_assert = Arc::clone(&host);
        let host_dyn: Arc<dyn RuntimeActionHost> = host;
        let runtime = tokio::runtime::Handle::current();
        let bridge = ActionBridge::new(runtime, host_dyn);
        let user_data = bridge.user_data();

        // Invalidate (as a `/reload` would on the old bridge).
        bridge.invalidate();
        assert!(!bridge.is_active());

        let args_ref = StbStringRef::from_str("{}");
        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            RuntimeActionId::GetSystemPrompt.into(),
            args_ref,
            &mut out as *mut StbString,
            user_data,
        );
        // Nonzero (error), and the host method never ran (no dispatch).
        assert_eq!(rc, 1, "stale bridge ⇒ error return code");
        // SAFETY: the action wrote a live host-owned output, freed below.
        let json_text = unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
        }
        .to_string_lossy();
        assert!(
            json_text.contains("stale"),
            "stale-bridge error payload: {json_text}"
        );

        assert!(
            host_for_assert.saw.lock().unwrap().is_empty(),
            "host dispatch must NOT run on a stale bridge"
        );
    }

    #[tokio::test]
    async fn trampoline_rejects_unknown_numeric_id_without_dispatch() {
        let host = Arc::new(MockHost {
            prompt: String::new(),
            saw: Mutex::new(Vec::new()),
        });
        let host_for_assert = Arc::clone(&host);
        let host_dyn: Arc<dyn RuntimeActionHost> = host;
        let bridge = ActionBridge::new(tokio::runtime::Handle::current(), host_dyn);
        let user_data = bridge.user_data();

        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            0xFFFF_FFFE,
            StbStringRef::from_str("{}"),
            &mut out,
            user_data,
        );

        assert_eq!(rc, 2, "unknown action id must be a protocol error");
        let payload: serde_json::Value = serde_json::from_str(
            &unsafe {
                rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
            }
            .to_string_lossy(),
        )
        .expect("structured error JSON");
        assert_eq!(payload["error"], "unknown runtime action id 4294967294");

        assert!(
            host_for_assert.saw.lock().unwrap().is_empty(),
            "unknown action id must not reach host dispatch"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn v2_trampoline_accepts_new_action() {
        let host = Arc::new(MockHost {
            prompt: "v2".to_string(),
            saw: Mutex::new(Vec::new()),
        });
        let host_for_assert = Arc::clone(&host);
        let host_dyn: Arc<dyn RuntimeActionHost> = host;
        let bridge = ActionBridge::new(tokio::runtime::Handle::current(), host_dyn);
        let user_data = bridge.user_data();

        let mut v2_out = StbString::empty();
        assert_eq!(
            trampoline_runtime_action(
                RuntimeActionId::GetCliFlag.into(),
                StbStringRef::from_str(r#"{"name":"flag"}"#),
                &mut v2_out,
                user_data,
            ),
            0
        );
        // SAFETY: this action returned a live host allocation, not previously freed.
        drop(unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(v2_out, Some(crate::host_free_string))
        });

        assert_eq!(
            *host_for_assert.saw.lock().unwrap(),
            vec![RuntimeActionId::GetCliFlag]
        );
    }

    /// `ReloadMailbox` + `reload_callback_from_mailbox` round-trip: signalling
    /// fires the installed receiver; an uninstalled mailbox yields `Err` (the
    /// host's "not configured" fallback). Exercises the B5d reload-signal path
    /// the TUI installs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reload_mailbox_signals_installed_receiver() {
        let mailbox = ReloadMailbox::new();
        // No receiver installed yet ⇒ signal fails.
        assert!(matches!(mailbox.signal(), Err(())));

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        mailbox.install(tx);
        let cb = reload_callback_from_mailbox(mailbox.clone());
        cb().await;
        assert_eq!(
            rx.recv().await,
            Some(()),
            "installed receiver saw the signal"
        );
        mailbox.clear();
    }

    // ---- UiDialogMailbox (runtime action 17 / ask_user) ----

    fn open_args(id: &str, tool_call: &str) -> serde_json::Value {
        serde_json::json!({
            "op": "open",
            "requestId": id,
            "toolCallId": tool_call,
            "ui": {"kind": "selector", "questions": [
                {"id": "q1", "question": "Which target?", "options": ["Linux"]}
            ]},
        })
    }

    /// Headless hosts (nothing attached) must fail loudly rather than fabricate
    /// a user answer.
    #[test]
    fn ui_dialog_open_requires_an_attached_ui() {
        let mailbox = UiDialogMailbox::new();
        let error = mailbox.handle(open_args("r1", "c1")).unwrap_err();
        assert!(error.contains("interactive UI"), "got: {error}");
    }

    #[test]
    fn ui_dialog_open_poll_answer_round_trip() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();

        let opened = mailbox.handle(open_args("r1", "c1")).unwrap();
        assert_eq!(opened["status"], "pending");
        assert_eq!(opened["toolCallId"], "c1");

        let request = mailbox.take_pending().expect("request visible to TUI");
        assert_eq!(request.request_id, "r1");
        assert_eq!(request.tool_call_id.as_deref(), Some("c1"));
        // A pending poll is non-destructive and remains available.
        assert_eq!(mailbox.poll("r1").unwrap()["status"], "pending");
        assert_eq!(mailbox.poll("r1").unwrap()["status"], "pending");

        mailbox
            .respond("r1", serde_json::json!("7890"))
            .expect("respond");
        let answered = mailbox.poll("r1").unwrap();
        assert_eq!(answered["status"], "answered");
        assert_eq!(answered["answer"], "7890");
        // Polling consumes the terminal entry.
        assert!(mailbox.poll("r1").is_err());
    }

    #[test]
    fn ui_dialog_cancel_is_reported_once() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        mailbox.handle(open_args("r1", "c1")).unwrap();
        mailbox.cancel("r1").unwrap();
        let cancelled = mailbox.poll("r1").unwrap();
        assert_eq!(cancelled["status"], "cancelled");
        assert!(mailbox.poll("r1").is_err());
    }

    #[test]
    fn ui_dialog_tool_cancel_preserves_other_calls_and_rejects_late_questions() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        mailbox.handle(open_args("r1", "c1")).unwrap();
        mailbox.handle(open_args("r2", "c2")).unwrap();
        mailbox.cancel_tool_call("c1");
        assert_eq!(mailbox.poll("r1").unwrap()["status"], "cancelled");
        assert_eq!(mailbox.take_pending().unwrap().request_id, "r2");
        assert_eq!(mailbox.poll("r2").unwrap()["status"], "pending");
        assert_eq!(
            mailbox.handle(open_args("late", "c1")).unwrap()["status"],
            "cancelled"
        );
        assert!(mailbox.take_pending().is_none());
    }

    #[test]
    fn ui_dialog_concurrent_requests_do_not_cross_answers() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        mailbox.handle(open_args("r1", "c1")).unwrap();
        mailbox.handle(open_args("r2", "c2")).unwrap();

        let first = mailbox.take_pending().unwrap().request_id;
        let second = mailbox.take_pending().unwrap().request_id;
        assert_ne!(first, second);

        // Answer the SECOND request first; the first must stay pending.
        mailbox
            .respond(&second, serde_json::json!("second"))
            .unwrap();
        assert_eq!(mailbox.poll(&first).unwrap()["status"], "pending");
        let second_answer = mailbox.poll(&second).unwrap();
        assert_eq!(second_answer["answer"], "second");

        mailbox.respond(&first, serde_json::json!("first")).unwrap();
        assert_eq!(mailbox.poll(&first).unwrap()["answer"], "first");
    }

    #[test]
    fn ui_dialog_detach_cancels_outstanding() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        mailbox.handle(open_args("r1", "c1")).unwrap();
        mailbox.detach();
        assert!(!mailbox.is_attached());
        assert_eq!(mailbox.pending_len(), 0);
        // Re-opening without a consumer is an explicit error again.
        assert!(mailbox.handle(open_args("r2", "c2")).is_err());
        assert_eq!(mailbox.poll("r1").unwrap()["status"], "cancelled");
    }

    #[test]
    fn ui_dialog_duplicate_open_does_not_reenqueue() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        mailbox.handle(open_args("r1", "c1")).unwrap();
        let again = mailbox.handle(open_args("r1", "c1")).unwrap();
        assert_eq!(again["status"], "pending");
        assert_eq!(mailbox.pending_len(), 1);
        mailbox.take_pending().unwrap();
        assert!(mailbox.take_pending().is_none());
    }

    /// `ListCommands` must route through the trampoline to the host method, and
    /// the default implementation must answer with an **empty list** rather than
    /// an error — a client has to be able to tell "no commands registered" apart
    /// from "this host does not support the action".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_commands_routes_to_the_host_and_defaults_to_empty() {
        let host: Arc<dyn RuntimeActionHost> = Arc::new(MockHost {
            prompt: String::new(),
            saw: Mutex::new(Vec::new()),
        });
        let bridge = ActionBridge::with_ui_dialog(
            tokio::runtime::Handle::current(),
            host,
            UiDialogMailbox::new(),
        );
        let user_data = bridge.user_data();
        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            u32::from(RuntimeActionId::ListCommands),
            StbStringRef::from_str("{}"),
            &mut out,
            user_data,
        );
        assert_eq!(
            rc, 0,
            "ListCommands must not fail on a host with no registry"
        );
        let payload: serde_json::Value = serde_json::from_str(
            &unsafe {
                rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
            }
            .to_string_lossy(),
        )
        .expect("JSON");
        assert_eq!(
            payload
                .get("commands")
                .and_then(|c| c.as_array())
                .map(Vec::len),
            Some(0),
            "an empty registry is an empty list, not an error: {payload}"
        );
    }

    /// The v2 trampoline must route action 17 into the session mailbox rather
    /// than rejecting it as unknown, and a detached mailbox must surface the
    /// explicit "requires an interactive UI" error (headless contract).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn v2_dispatch_routes_ui_dialog_action_17() {
        let host: Arc<dyn RuntimeActionHost> = Arc::new(MockHost {
            prompt: String::new(),
            saw: Mutex::new(Vec::new()),
        });
        let bridge = ActionBridge::with_ui_dialog(
            tokio::runtime::Handle::current(),
            host,
            UiDialogMailbox::new(),
        );
        let user_data = bridge.user_data();

        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            17,
            StbStringRef::from_str(r#"{"op":"open","requestId":"r1","ui":{}}"#),
            &mut out,
            user_data,
        );
        assert_eq!(rc, 1);
        let payload: serde_json::Value = serde_json::from_str(
            &unsafe {
                rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
            }
            .to_string_lossy(),
        )
        .expect("structured error JSON");
        assert!(
            payload["error"]
                .as_str()
                .is_some_and(|error| error.contains("interactive UI")),
            "unexpected payload: {payload}"
        );

        // A genuinely unknown id is still rejected (no silent acceptance).
        // Derive "one past the highest" from the enum so adding an action does
        // not silently turn this into a real dispatch.
        let one_past_last = rpi_plugin_sdk::RuntimeActionId::MAX + 1;
        let mut out = StbString::empty();
        assert_eq!(
            trampoline_runtime_action(
                one_past_last,
                StbStringRef::from_str("{}"),
                &mut out,
                user_data
            ),
            2
        );
        // SAFETY: this action returned a live host allocation, not previously freed.
        drop(unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
        });
    }

    #[tokio::test(flavor = "current_thread")]
    async fn component_bridge_isolates_plugins_and_cleans_up_reload_and_drop() {
        let host = Arc::new(MockHost {
            prompt: String::new(),
            saw: Mutex::new(Vec::new()),
        });
        let root = ActionBridge::new(tokio::runtime::Handle::current(), host);
        let a = root.for_plugin("a");
        let b = root.for_plugin("b");
        let mailbox = root.extension_status_mailbox();
        for plugin in [&a, &b] {
            let mut out = StbString::empty();
            assert_eq!(
                trampoline_runtime_action(
                    22,
                    StbStringRef::from_str(
                        r#"{"op":"register","id":"same","slot":"footer.after"}"#
                    ),
                    &mut out,
                    plugin.user_data()
                ),
                0
            );
            // SAFETY: this action returned a live host allocation, not previously freed.
            drop(unsafe {
                rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
            });
        }
        assert_eq!(mailbox.panels().len(), 2);
        drop(a);
        assert_eq!(mailbox.panels().len(), 1);
        root.invalidate();
        assert!(mailbox.panels().is_empty());
    }

    /// Action 28 is mailbox-only, so it must not need a Tokio worker.
    ///
    /// The route must be off-executor: on a current-thread bridge a spawn-based
    /// route parks the only thread able to finish the call. The timeout is the
    /// regression.
    #[test]
    fn relay_ui_dialog_runs_off_the_executor() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        let out = dispatch_relay_ui_dialog(&mailbox, serde_json::json!({"op":"release"}))
            .expect("release is a supported op");
        assert_eq!(out["requeued"], serde_json::json!(0));
    }

    /// Action 18 (`SetStatus`) is bridge-local, like `UiDialog`: it writes the
    /// shared registry the TUI polls and never touches the harness.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn v2_dispatch_routes_set_status_action_18() {
        let host: Arc<dyn RuntimeActionHost> = Arc::new(MockHost {
            prompt: String::new(),
            saw: Mutex::new(Vec::new()),
        });
        let status = crate::status::ExtensionStatusMailbox::new();
        let bridge = ActionBridge::with_reload_and_ui(
            tokio::runtime::Handle::current(),
            host,
            Arc::new(|| Box::pin(async {})),
            UiDialogMailbox::new(),
            status.clone(),
            crate::editor_text::EditorTextMailbox::new(),
        );
        let user_data = bridge.user_data();

        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            18,
            StbStringRef::from_str(r#"{"key":"langfuse","value":"langfuse ✓ (trace sent)"}"#),
            &mut out,
            user_data,
        );
        assert_eq!(rc, 0);
        assert_eq!(status.text(), "langfuse ✓ (trace sent)");
        // SAFETY: this action returned a live host allocation, not previously freed.
        drop(unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
        });

        // Clearing through the same action empties the line again.
        let mut out = StbString::empty();
        assert_eq!(
            trampoline_runtime_action(
                18,
                StbStringRef::from_str(r#"{"key":"langfuse","value":""}"#),
                &mut out,
                user_data
            ),
            0
        );
        assert_eq!(status.text(), "");
        // SAFETY: this action returned a live host allocation, not previously freed.
        drop(unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
        });

        // A missing key is a host-level error (rc=1), not an unknown action.
        let mut out = StbString::empty();
        assert_eq!(
            trampoline_runtime_action(18, StbStringRef::from_str("{}"), &mut out, user_data),
            1
        );
        // SAFETY: this action returned a live host allocation, not previously freed.
        drop(unsafe {
            rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
        });
    }
    /// The FFI trampoline's ceiling must be exactly the SDK's highest action id.
    ///
    /// A branch that adds an action but leaves the ceiling behind compiles,
    /// keeps every other test green, and quietly makes the newest action
    /// un-dispatchable. The ceiling and the enum live in different crates, so
    /// nothing but a test can notice the drift.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn trampoline_ceiling_matches_the_highest_defined_action() {
        let highest = RuntimeActionId::MAX;
        assert_eq!(
            highest,
            RuntimeActionId::ALL
                .iter()
                .copied()
                .map(u32::from)
                .max()
                .unwrap(),
            "RuntimeActionId::MAX must be the maximum of RuntimeActionId::ALL"
        );
        let host: Arc<dyn RuntimeActionHost> = Arc::new(MockHost {
            prompt: String::new(),
            saw: Mutex::new(Vec::new()),
        });
        let bridge = ActionBridge::with_ui_dialog(
            tokio::runtime::Handle::current(),
            host,
            UiDialogMailbox::new(),
        );
        let mut out = StbString::empty();
        let rc = trampoline_runtime_action(
            highest,
            StbStringRef::from_str("{}"),
            &mut out,
            bridge.user_data(),
        );
        assert_ne!(
            rc, 2,
            "the highest defined action id ({highest}) must not be rejected as unknown — \
             the trampoline ceiling is stale"
        );
        if rc == 0 {
            // SAFETY: success means the trampoline wrote a live host allocation.
            drop(unsafe {
                rpi_plugin_sdk::callbacks::OwnedString::from_raw(out, Some(crate::host_free_string))
            });
        }
    }
}

impl Drop for ActionBridge {
    fn drop(&mut self) {
        bridges()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, entry| !std::ptr::eq(entry.bridge.as_ptr(), self));
        if self.invocation.is_some() {
            return;
        }
        if let Some((owner, _)) = &self.plugin_identity {
            self.ext_status.remove_owner(*owner);
            self.host.cancel_child_agents(*owner);
            self.host.cancel_side_questions(*owner);
        }
    }
}

#[cfg(test)]
mod runtime_tests;

#[cfg(test)]
mod relay_tests {
    use super::*;

    fn request(id: &str) -> UiDialogRequest {
        UiDialogRequest {
            request_id: id.to_string(),
            tool_call_id: Some(format!("tool-{id}")),
            ui: serde_json::json!({"question": "ok?"}),
            raw: serde_json::json!({}),
        }
    }

    /// A reserved request must be invisible to the TUI but visible to the plugin
    /// that reserved it. Otherwise a remote UI's question pops up in the
    /// terminal while the user who can answer it is watching the remote client.
    #[test]
    fn reserved_requests_are_invisible_to_the_tui() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        let reserved = format!("{PLUGIN_RESERVED_PREFIX}gate-1");
        mailbox
            .handle(serde_json::json!({
                "op": "open",
                "requestId": reserved,
                "toolCallId": "t1",
                "ui": {}
            }))
            .unwrap();
        mailbox
            .handle(serde_json::json!({
                "op": "open",
                "requestId": "plain-1",
                "toolCallId": "t2",
                "ui": {}
            }))
            .unwrap();

        // The TUI skips the reserved one and takes the plain one.
        let taken = mailbox
            .take_pending_filtered(|r| !mailbox.is_reserved(&r.request_id))
            .expect("the unreserved request must reach the TUI");
        assert_eq!(taken.request_id, "plain-1");

        // The plugin still sees its own.
        let own = mailbox
            .take_pending_filtered(|r| mailbox.is_reserved(&r.request_id))
            .expect("the plugin must still see its reserved request");
        assert_eq!(own.request_id, reserved);
    }

    /// Losing the relay consumer must not leave the TUI locked out forever.
    #[test]
    fn release_or_detach_clears_takeover_and_requeues_unanswered() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        mailbox
            .handle(serde_json::json!({
                "op": "open", "requestId": "plain-r", "toolCallId": "t", "ui": {}
            }))
            .unwrap();

        // Relay takes it, then gives up without answering.
        mailbox.set_external_takeover(true);
        assert_eq!(mailbox.take_all_unreserved().len(), 1);
        assert_eq!(mailbox.release_takeover(), 1);
        assert!(
            !mailbox.external_takeover(),
            "release must hand delivery back to the TUI"
        );
        // The question is visible again instead of being silently lost.
        assert_eq!(
            mailbox
                .take_pending_filtered(|r| !mailbox.is_reserved(&r.request_id))
                .map(|r| r.request_id),
            Some("plain-r".to_string())
        );

        // Detach releases ownership too, so a reattached TUI can consume.
        mailbox.set_external_takeover(true);
        mailbox.detach();
        assert!(!mailbox.external_takeover(), "detach must clear takeover");
    }

    /// Takeover and dequeue share one lock, so a relay cannot claim ownership
    /// in the gap and lose the question.
    #[test]
    fn tui_take_is_atomic_against_takeover() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        mailbox.set_external_takeover(true);
        mailbox
            .handle(serde_json::json!({
                "op": "open", "requestId": "plain-a", "toolCallId": "t", "ui": {}
            }))
            .unwrap();
        assert!(
            mailbox.take_pending_unless_external(|_| true).is_none(),
            "the TUI must stand down while a relay owns delivery"
        );
        // Still queued for the relay.
        assert_eq!(mailbox.take_all_unreserved().len(), 1);
    }

    /// `RelayUiDialog {op:"take"}` hands over everything no plugin reserved and
    /// leaves the reserved entries queued.
    #[test]
    fn relay_take_returns_unreserved_and_keeps_reserved() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        mailbox
            .handle(serde_json::json!({
                "op": "open",
                "requestId": format!("{PLUGIN_RESERVED_PREFIX}keep"),
                "toolCallId": "t1",
                "ui": {}
            }))
            .unwrap();
        mailbox
            .handle(serde_json::json!({
                "op": "open",
                "requestId": "relay-1",
                "toolCallId": "t2",
                "ui": {}
            }))
            .unwrap();

        let taken = mailbox.take_all_unreserved();
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].request_id, "relay-1");
        // The reserved one is still there for its owner.
        assert_eq!(mailbox.pending_len(), 1);
    }

    /// A taken request is still answerable — `take` moves it out of `pending`
    /// but keeps it in `active`.
    #[test]
    fn a_relayed_request_can_still_be_answered() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        mailbox
            .handle(serde_json::json!({
                "op": "open",
                "requestId": "relay-1",
                "toolCallId": "t1",
                "ui": {}
            }))
            .unwrap();
        let taken = mailbox.take_all_unreserved();
        assert_eq!(taken.len(), 1);

        mailbox
            .handle(serde_json::json!({
                "op": "respond",
                "requestId": "relay-1",
                "answer": {"ok": true}
            }))
            .unwrap();
        // Answered requests are not handed out again.
        assert_eq!(
            mailbox
                .take_pending_filtered(|_| true)
                .map(|r| r.request_id),
            None,
            "an answered request must not resurface"
        );
    }

    /// `take_pending()` (the un-filtered form) still takes everything, so a
    /// caller that wants the old behaviour keeps it.
    #[test]
    fn take_pending_still_takes_reserved_entries() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        let reserved = format!("{PLUGIN_RESERVED_PREFIX}x");
        mailbox
            .handle(serde_json::json!({
                "op": "open",
                "requestId": reserved,
                "toolCallId": "t1",
                "ui": {}
            }))
            .unwrap();
        assert_eq!(
            mailbox.take_pending().map(|r| r.request_id),
            Some(format!("{PLUGIN_RESERVED_PREFIX}x"))
        );
    }

    /// External takeover must make the TUI skip taking entirely.
    ///
    /// The two consumers draw from the same queue, so without an owner whichever
    /// polls first wins. A remote client that has taken over must not race a
    /// terminal nobody is watching.
    #[test]
    fn external_takeover_makes_the_tui_stand_down() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        mailbox
            .handle(serde_json::json!({
                "op": "open",
                "requestId": "plain-1",
                "toolCallId": "t1",
                "ui": {}
            }))
            .unwrap();

        assert!(!mailbox.external_takeover());
        mailbox.set_external_takeover(true);
        assert!(mailbox.external_takeover());

        // The TUI's take path returns nothing while the external UI owns it…
        let tui_take = |m: &UiDialogMailbox| {
            if m.external_takeover() {
                return None;
            }
            m.take_pending_filtered(|r| !m.is_reserved(&r.request_id))
        };
        assert!(
            tui_take(&mailbox).is_none(),
            "the TUI must not steal a request an external UI owns"
        );
        // …and the request is still there for the external consumer.
        assert_eq!(mailbox.take_all_unreserved().len(), 1);
    }

    /// Handing ownership back lets the TUI resume.
    #[test]
    fn clearing_takeover_returns_dialogs_to_the_tui() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        mailbox.set_external_takeover(true);
        mailbox.set_external_takeover(false);
        mailbox
            .handle(serde_json::json!({
                "op": "open",
                "requestId": "plain-2",
                "toolCallId": "t2",
                "ui": {}
            }))
            .unwrap();
        assert_eq!(
            mailbox
                .take_pending_filtered(|r| !mailbox.is_reserved(&r.request_id))
                .map(|r| r.request_id),
            Some("plain-2".to_string())
        );
    }

    #[test]
    fn request_helper_matches_the_prefix_rule() {
        let mailbox = UiDialogMailbox::new();
        mailbox.attach();
        assert!(mailbox.is_reserved(&request(PLUGIN_RESERVED_PREFIX).request_id));
        assert!(!mailbox.is_reserved(&request("plain").request_id));
    }
}
