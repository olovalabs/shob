//! Embeddable rpi sessions for a host UI.
//!
//! The host supplies a working directory plus an optional `provider/model`
//! selection and session id. This crate reuses `rpi-cli`'s startup path — model
//! and credential resolution, built-in tools, system prompt, JSONL session
//! storage and `AgentHarness` — but never spawns the `rpi` executable and never
//! speaks the RPC JSONL protocol.
//!
//! - [`EmbeddedSession`] owns one conversation on a background Tokio runtime.
//!   [`EmbeddedSession::poll`] drains typed [`SessionEvent`]s, so a host can keep
//!   its existing polling UI loop.
//! - [`catalog`] and [`providers`] replace `rpi --list-models` and `rpi auth
//!   check` with in-process reads of the same `models.json`, `auth.json`,
//!   `settings.json` and environment sources.
//! - [`Login`] replaces `rpi auth login` with native API-key, PKCE browser and
//!   device-code sign-in, while preserving extension-owned OAuth actions.
//!
//! Sessions run with `no_extensions` so several conversations can coexist in one
//! process without sharing rpi's process-global plugin session identity. Model
//! discovery and login still load extensions when they need an OAuth provider.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
#[cfg(test)]
use std::time::Duration;

use rpi_agent::events::AgentEvent as RpiEvent;
use rpi_agent::message::AgentMessage;
pub use rpi_agent::{BeforeToolCall, BeforeToolCallResult};
use rpi_ai::types::{AssistantMessageEvent, StopReason};
use rpi_cli::args::{Args, Mode};
use rpi_cli::session::BuildError;
use rpi_harness::agent_harness::AgentLane;
use rpi_runtime::config::Credential;
use rpi_runtime::{RuntimeConfig, RuntimeSettings, ValueSource};

mod antigravity;
pub const ANTIGRAVITY: &str = rpi_ai::providers::antigravity::ID;
pub use rpi_cli::native_auth::{AuthMethod, AuthMethods};
pub use rpi_cli::session::{task_list, user_input};

static EXTENSION_OAUTH_METHODS: std::sync::LazyLock<
    std::sync::RwLock<std::collections::BTreeMap<PathBuf, Arc<std::collections::BTreeSet<String>>>>,
> = std::sync::LazyLock::new(Default::default);

/// Cheap, side-effect-free native auth metadata without project-specific extensions.
pub fn auth_methods(provider: &str) -> AuthMethods {
    rpi_cli::native_auth::auth_methods(provider)
}

/// Pure cached metadata for a project path, including its discovered canonical alias.
pub fn cached_auth_methods(provider: &str, cwd: &Path) -> AuthMethods {
    let mut methods = auth_methods(provider);
    methods.oauth |= EXTENSION_OAUTH_METHODS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(cwd)
        .is_some_and(|providers| providers.contains(provider));
    methods
}

fn auth_runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    static RUNTIME: std::sync::LazyLock<Result<tokio::runtime::Runtime, String>> =
        std::sync::LazyLock::new(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("kathamo-auth-extensions")
                .enable_all()
                .build()
                .map_err(|_| "Could not initialize extension authentication runtime".into())
        });
    RUNTIME.as_ref().map_err(Clone::clone)
}

/// Refresh extension metadata outside a UI render pass, for the selected project.
pub fn refresh_auth_methods(cwd: &Path) -> Result<(), String> {
    let canonical = cwd
        .canonicalize()
        .map_err(|_| "Could not resolve authentication project directory".to_string())?;
    let _runtime = auth_runtime()?.enter();
    let prepared = rpi_cli::session::prepare_extensions(
        &Args::default(),
        &canonical,
        project_trusted(&canonical),
    )
    .map_err(|error| format!("Could not discover extension authentication: {error}"))?;
    let providers = prepared
        .extension_session
        .snapshot()
        .map(rpi_extensions::list_oauth_providers)
        .unwrap_or_default();
    let methods = Arc::new(providers.into_iter().map(|provider| provider.id).collect());
    let mut cache = EXTENSION_OAUTH_METHODS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.insert(canonical, Arc::clone(&methods));
    cache.insert(cwd.to_path_buf(), methods);
    Ok(())
}

/// Explicitly discover extension-owned OAuth for a project before selecting a method.
pub fn auth_methods_for_cwd(provider: &str, cwd: &Path) -> AuthMethods {
    if !rpi_cli::native_auth::supports(provider) && provider != ANTIGRAVITY {
        let _ = refresh_auth_methods(cwd);
    }
    cached_auth_methods(provider, cwd)
}

/// One catalog row, matching the identity Kathamo shows in its model picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub provider: String,
    pub id: String,
    pub thinking_levels: Vec<String>,
}

fn model_thinking_levels(model: &rpi_ai::Model) -> Vec<String> {
    if !model.reasoning {
        return Vec::new();
    }
    model
        .supported_thinking_levels()
        .into_iter()
        .filter(|level| {
            !matches!(
                level,
                rpi_ai::ThinkingLevel::Off | rpi_ai::ThinkingLevel::Minimal
            )
        })
        .filter_map(|level| {
            serde_json::to_value(level)
                .ok()?
                .as_str()
                .map(str::to_owned)
        })
        .collect()
}

/// Local provider readiness. No credential material is exposed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderState {
    pub id: String,
    pub ready: bool,
    pub source: Option<String>,
}

/// Read the configured model catalog in-process (`models.json` + built-ins).
pub fn catalog() -> Result<Vec<ModelInfo>, String> {
    catalog_filtered(false)
}

fn catalog_filtered(chat_only: bool) -> Result<Vec<ModelInfo>, String> {
    rpi_cli::provider::catalog_all()
        .map(|models| {
            let mut rows: Vec<_> = models
                .into_iter()
                .filter(|model| !chat_only || rpi_runtime::catalog::is_chat_model(model))
                .map(|model| ModelInfo {
                    thinking_levels: model_thinking_levels(&model),
                    provider: model.provider,
                    id: model.id,
                })
                .collect();
            rows.retain(|model| model.provider != ANTIGRAVITY);
            rows.extend(antigravity::models().into_iter().map(|model| {
                ModelInfo {
                    // This adapter maps every request to these three wire tiers.
                    thinking_levels: ["low", "medium", "high"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                    provider: ANTIGRAVITY.into(),
                    id: model.id,
                }
            }));
            rows
        })
        .map_err(|error| error.to_string())
}

/// Chat-capable routes for an agent conversation. The full [`catalog`] keeps
/// non-chat identities and their backend capability metadata discoverable.
pub fn chat_catalog() -> Result<Vec<ModelInfo>, String> {
    catalog_filtered(true)
}

/// Report local readiness for every provider known to the model/credential
/// stores, using the same rules as execution.
pub fn providers() -> Result<Vec<ProviderState>, String> {
    let config = RuntimeConfig {
        models: rpi_cli::config::load_models_config().map_err(|error| error.to_string())?,
        credentials: rpi_cli::config::read_auth().map_err(|error| error.to_string())?,
        settings: RuntimeSettings::default(),
    };
    let values = EnvironmentValues;
    let registry = rpi_cli::provider::runtime_factories(None);
    let states = config.auth_states(&registry, &values);
    let mut rows: Vec<_> = states
        .into_iter()
        .map(|(id, state)| ProviderState {
            id,
            ready: state.is_ready(),
            source: Some(state.source.as_json_str().to_string()),
        })
        .collect();
    rows.retain(|provider| provider.id != ANTIGRAVITY);
    let ready = antigravity::ready();
    rows.push(ProviderState {
        id: ANTIGRAVITY.into(),
        ready,
        source: ready.then(|| "Google OAuth".into()),
    });
    Ok(rows)
}

static TOOL_ARRIVAL_HOOK: OnceLock<fn(&str)> = OnceLock::new();
static TOOL_END_HOOK: OnceLock<fn(&str)> = OnceLock::new();
static TOOL_OUTPUT_HOOK: OnceLock<fn(&str)> = OnceLock::new();

/// Install a host callback invoked before tool-start raw queue admission.
/// Hosts use it to correlate transport arrival with the first painted card.
/// It never changes agent behavior.
pub fn set_tool_arrival_hook(hook: fn(&str)) {
    let _ = TOOL_ARRIVAL_HOOK.set(hook);
}

/// Install a host callback invoked before tool-end raw queue admission.
pub fn set_tool_end_hook(hook: fn(&str)) {
    let _ = TOOL_END_HOOK.set(hook);
}

/// Install metadata-only timing for the first nonempty tool snapshot before
/// raw host-queue admission. Does not include tool-internal capture latency.
pub fn set_tool_output_hook(hook: fn(&str)) {
    let _ = TOOL_OUTPUT_HOOK.set(hook);
}

/// Usage delta for one finalized assistant message. Unknown pricing is not free.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AgentUsage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cost: Option<f64>,
}

/// Events delivered to the host. This mirrors Kathamo's existing chat event
/// consumer so the subprocess bridge can be replaced without changing the UI.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEvent {
    Text(String),
    Thinking(String),
    Status(String),
    Usage(AgentUsage),
    ToolStart {
        id: Option<String>,
        name: String,
        input: String,
    },
    ToolUpdate {
        id: Option<String>,
        name: String,
        output: String,
    },
    ToolEnd {
        id: Option<String>,
        name: String,
        output: String,
        failed: bool,
    },
    Error(String),
    Done,
    Closed,
}

/// Payload accounting and safe cumulative replacement for the host transport.
pub trait QueuedEvent {
    fn queued_bytes(&self) -> usize;
    fn is_snapshot(&self) -> bool;
    fn supersedes(&self, previous: &Self) -> bool;
}

/// Lossless, byte- and count-budgeted host transport. A single oversized record
/// is admitted exclusively: final tool results are never silently truncated.
/// Async producers await capacity; only dedicated pipe-reader threads may use
/// `send_blocking`. Neither method holds a lock while waiting.
pub struct EventQueue<E> {
    state: Mutex<EventQueueState<E>>,
    ready: tokio::sync::Notify,
    available: tokio::sync::Notify,
    space: std::sync::Condvar,
    timing: Option<QueueTiming>,
}

struct EventQueueState<E> {
    events: std::collections::VecDeque<E>,
    bytes: usize,
    closed: bool,
    enqueued: Option<std::collections::VecDeque<std::time::Instant>>,
}

type PhaseHook = Arc<dyn Fn(&'static str, std::time::Duration) + Send + Sync>;

struct QueueTiming {
    hook: PhaseHook,
    admission: &'static str,
    lock: &'static str,
    backpressure: &'static str,
    residence: &'static str,
    snapshot_dropped: &'static str,
}

pub const EVENT_QUEUE_RECORDS: usize = 256;
pub const EVENT_QUEUE_BYTES: usize = 4 * 1024 * 1024;
pub const POLL_RECORDS: usize = 64;
pub const POLL_BYTES: usize = 256 * 1024;

impl<E: QueuedEvent> Default for EventQueue<E> {
    fn default() -> Self {
        Self {
            state: Mutex::new(EventQueueState {
                events: Default::default(),
                bytes: 0,
                closed: false,
                enqueued: None,
            }),
            ready: tokio::sync::Notify::new(),
            available: tokio::sync::Notify::new(),
            space: std::sync::Condvar::new(),
            timing: None,
        }
    }
}

impl<E: QueuedEvent> EventQueue<E> {
    fn timed(hook: Option<&PhaseHook>, raw: bool) -> Self {
        let mut queue = Self::default();
        // Clock reads and timestamp storage are absent during normal operation.
        if std::env::var_os("KATHAMO_STREAM_TIMINGS").is_some_and(|value| value != "0") {
            if let Some(hook) = hook {
                queue.timing = Some(QueueTiming {
                    hook: Arc::clone(hook),
                    admission: if raw {
                        "queue.raw.admission"
                    } else {
                        "queue.projected.admission"
                    },
                    lock: if raw {
                        "queue.raw.lock"
                    } else {
                        "queue.projected.lock"
                    },
                    backpressure: if raw {
                        "queue.raw.backpressure"
                    } else {
                        "queue.projected.backpressure"
                    },
                    residence: if raw {
                        "queue.raw.residence"
                    } else {
                        "queue.projected.residence"
                    },
                    snapshot_dropped: if raw {
                        "queue.raw.snapshot_dropped"
                    } else {
                        "queue.projected.snapshot_dropped"
                    },
                });
                queue
                    .state
                    .get_mut()
                    .unwrap_or_else(|e| e.into_inner())
                    .enqueued = Some(Default::default());
            }
        }
        queue
    }

    fn admission_finished(
        &self,
        started: Option<std::time::Instant>,
        blocked: bool,
        lock_wait: std::time::Duration,
    ) {
        if let (Some(timing), Some(started)) = (&self.timing, started) {
            let elapsed = started.elapsed();
            (timing.hook)(timing.admission, elapsed);
            (timing.hook)(timing.lock, lock_wait);
            if blocked {
                (timing.hook)(timing.backpressure, elapsed);
            }
        }
    }

    fn lock_state(
        &self,
    ) -> (
        std::sync::MutexGuard<'_, EventQueueState<E>>,
        std::time::Duration,
    ) {
        let started = self.timing.as_ref().map(|_| std::time::Instant::now());
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (
            guard,
            started.map_or(std::time::Duration::ZERO, |started| started.elapsed()),
        )
    }

    fn insert(state: &mut EventQueueState<E>, event: E) -> Result<(), E> {
        if state.closed {
            return Err(event);
        }
        let replaced = if event.is_snapshot() {
            state
                .events
                .iter()
                .enumerate()
                .rev()
                .take_while(|(_, old)| old.is_snapshot())
                .find(|(_, old)| event.supersedes(old))
                .map(|(index, old)| (index, old.queued_bytes()))
        } else {
            None
        };
        let size = event.queued_bytes();
        let old_size = replaced.map_or(0, |(_, size)| size);
        let count = state.events.len() - usize::from(replaced.is_some());
        let bytes = state.bytes - old_size;
        if count != 0
            && (count >= EVENT_QUEUE_RECORDS || bytes.saturating_add(size) > EVENT_QUEUE_BYTES)
        {
            return Err(event);
        }
        if let Some((index, _)) = replaced {
            state.events.remove(index);
            if let Some(enqueued) = &mut state.enqueued {
                enqueued.remove(index);
            }
        }
        state.bytes = bytes.saturating_add(size);
        state.events.push_back(event);
        if let Some(enqueued) = &mut state.enqueued {
            enqueued.push_back(std::time::Instant::now());
        }
        Ok(())
    }

    pub async fn send(&self, mut event: E) -> Result<(), ()> {
        let started = self.timing.as_ref().map(|_| std::time::Instant::now());
        let mut blocked = false;
        let mut lock_wait = std::time::Duration::ZERO;
        loop {
            let notified = self.ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let (mut state, elapsed) = self.lock_state();
                lock_wait += elapsed;
                if state.closed {
                    return Err(());
                }
                match Self::insert(&mut state, event) {
                    Ok(()) => {
                        drop(state);
                        self.available.notify_one();
                        self.admission_finished(started, blocked, lock_wait);
                        return Ok(());
                    }
                    Err(pending) => event = pending,
                    // The next wait yields without holding the queue mutex.
                }
            }
            blocked = true;
            notified.await;
        }
    }

    pub fn send_blocking(&self, mut event: E) -> Result<(), ()> {
        let started = self.timing.as_ref().map(|_| std::time::Instant::now());
        let mut blocked = false;
        let (mut state, lock_wait) = self.lock_state();
        loop {
            if state.closed {
                return Err(());
            }
            match Self::insert(&mut state, event) {
                Ok(()) => {
                    drop(state);
                    self.available.notify_one();
                    self.admission_finished(started, blocked, lock_wait);
                    return Ok(());
                }
                Err(pending) => event = pending,
            }
            blocked = true;
            state = self.space.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Nonblocking admission for replaceable progress snapshots only.
    pub fn try_send(&self, event: E) -> Result<(), E> {
        let started = self.timing.as_ref().map(|_| std::time::Instant::now());
        let (mut state, lock_wait) = self.lock_state();
        let result = Self::insert(&mut state, event);
        drop(state);
        self.admission_finished(started, false, lock_wait);
        if result.is_ok() {
            self.available.notify_one();
        } else if let Some(timing) = &self.timing {
            (timing.hook)(timing.snapshot_dropped, std::time::Duration::ZERO);
        }
        result
    }

    async fn recv(&self) -> Option<E> {
        loop {
            let notified = self.available.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(event) = state.events.pop_front() {
                    state.bytes -= event.queued_bytes();
                    let enqueued = state.enqueued.as_mut().and_then(|times| times.pop_front());
                    drop(state);
                    self.ready.notify_waiters();
                    self.space.notify_all();
                    if let (Some(timing), Some(enqueued)) = (&self.timing, enqueued) {
                        (timing.hook)(timing.residence, enqueued.elapsed());
                    }
                    return Some(event);
                }
                if state.closed {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// A fair batch plus an explicit backlog flag. Consumers must resignal
    /// their coalesced host wake when the second return value is true.
    pub fn drain(&self) -> (Vec<E>, bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut events = Vec::new();
        let mut bytes = 0usize;
        let mut residence = Vec::new();
        while events.len() < POLL_RECORDS {
            let Some(front) = state.events.front() else {
                break;
            };
            let size = front.queued_bytes();
            if !events.is_empty() && bytes.saturating_add(size) > POLL_BYTES {
                break;
            }
            bytes = bytes.saturating_add(size);
            state.bytes -= size;
            events.push(state.events.pop_front().expect("front exists"));
            if let Some(enqueued) = state.enqueued.as_mut().and_then(|times| times.pop_front()) {
                residence.push(enqueued.elapsed());
            }
        }
        let more = !state.events.is_empty();
        drop(state);
        self.ready.notify_waiters();
        self.space.notify_all();
        if let Some(timing) = &self.timing {
            for elapsed in residence {
                (timing.hook)(timing.residence, elapsed);
            }
        }
        (events, more)
    }

    pub fn close(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
        self.ready.notify_waiters();
        self.space.notify_all();
        self.available.notify_waiters();
    }
}

impl QueuedEvent for SessionEvent {
    fn queued_bytes(&self) -> usize {
        match self {
            Self::Text(text) | Self::Thinking(text) | Self::Status(text) | Self::Error(text) => {
                text.len()
            }
            Self::ToolStart { id, name, input } => {
                id.as_ref().map_or(0, String::len) + name.len() + input.len()
            }
            Self::ToolUpdate { id, name, output }
            | Self::ToolEnd {
                id, name, output, ..
            } => id.as_ref().map_or(0, String::len) + name.len() + output.len(),
            Self::Usage(_) => std::mem::size_of::<AgentUsage>(),
            Self::Done | Self::Closed => 0,
        }
    }
    fn is_snapshot(&self) -> bool {
        matches!(self, Self::ToolUpdate { .. })
    }
    fn supersedes(&self, previous: &Self) -> bool {
        matches!((self, previous),
            (Self::ToolUpdate { id: Some(id), name, .. },
             Self::ToolUpdate { id: Some(old_id), name: old_name, .. })
                if id == old_id && name == old_name)
    }
}

/// Retain shared snapshots before expensive string projection. Text/structural
/// events await admission; synchronous progress replaces identified snapshots
/// or skips an intermediate snapshot under pressure. Final results are lossless.
enum PendingEvent {
    Ready(SessionEvent),
    Update {
        id: String,
        name: String,
        result: Arc<rpi_agent::AgentToolResult>,
        bytes: usize,
    },
    End {
        id: String,
        name: String,
        result: rpi_agent::AgentToolResult,
        failed: bool,
        bytes: usize,
    },
}

impl QueuedEvent for PendingEvent {
    fn queued_bytes(&self) -> usize {
        match self {
            Self::Ready(event) => event.queued_bytes(),
            Self::Update { bytes, .. } | Self::End { bytes, .. } => *bytes,
        }
    }
    fn is_snapshot(&self) -> bool {
        matches!(self, Self::Update { .. })
    }
    fn supersedes(&self, old: &Self) -> bool {
        matches!((self, old),
            (Self::Update { id, name, .. }, Self::Update { id: old_id, name: old_name, .. })
                if id == old_id && name == old_name)
    }
}

fn result_bytes(result: &rpi_agent::AgentToolResult) -> usize {
    result.content.capacity() * std::mem::size_of::<rpi_agent::TextContentOrImage>()
        + result
            .content
            .iter()
            .map(|content| match content {
                rpi_agent::TextContentOrImage::Text(text) => text.text.capacity(),
                rpi_agent::TextContentOrImage::Image(image) => {
                    image.data.len() + image.mime_type.capacity()
                }
            })
            .sum::<usize>()
        + json_bytes(&result.details)
        + result.added_tool_names.capacity() * std::mem::size_of::<String>()
        + result
            .added_tool_names
            .iter()
            .map(String::capacity)
            .sum::<usize>()
}

fn json_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(text) => text.len() + 32,
        serde_json::Value::Array(values) => {
            values.iter().map(json_bytes).sum::<usize>() + values.len() * 32
        }
        serde_json::Value::Object(values) => values
            .iter()
            .map(|(key, value)| key.len() + json_bytes(value) + 64)
            .sum(),
        _ => 32,
    }
}

impl PendingEvent {
    fn into_session(self) -> SessionEvent {
        match self {
            Self::Ready(event) => event,
            Self::Update {
                id, name, result, ..
            } => SessionEvent::ToolUpdate {
                id: Some(id),
                name,
                output: tool_result_text(&result),
            },
            Self::End {
                id,
                name,
                result,
                failed,
                ..
            } => SessionEvent::ToolEnd {
                id: Some(id),
                name,
                output: tool_result_text(&result),
                failed,
            },
        }
    }
}

struct HostEmitter {
    pending: Arc<EventQueue<PendingEvent>>,
    delta_arrival: Option<Arc<dyn Fn(bool) + Send + Sync>>,
}

impl rpi_agent::AgentEmitter for HostEmitter {
    fn emit(&self, event: RpiEvent) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        let pending = Arc::clone(&self.pending);
        let usage = finalized_usage(&event);
        let event = pending_event(event);
        if let (Some(hook), Some(PendingEvent::Ready(event))) = (&self.delta_arrival, &event) {
            match event {
                SessionEvent::Text(text) if !text.is_empty() => hook(false),
                SessionEvent::Thinking(text) if !text.is_empty() => hook(true),
                _ => {}
            }
        }
        Box::pin(async move {
            if let Some(usage) = usage {
                if pending
                    .send(PendingEvent::Ready(SessionEvent::Usage(usage)))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            if let Some(event) = event {
                let _ = pending.send(event).await;
            }
        })
    }
    fn try_emit(&self, event: RpiEvent) {
        if matches!(event, RpiEvent::ToolExecutionUpdate { .. }) {
            if let Some(event) = pending_event(event) {
                let _ = self.pending.try_send(event);
            }
        }
    }
}

/// Inputs for one embedded conversation.
#[derive(Clone)]
pub struct SessionOptions {
    pub cwd: PathBuf,
    /// Optional host-owned root for durable session files.
    pub session_dir: Option<PathBuf>,
    /// Provider id from the host's model picker.
    pub provider: Option<String>,
    /// Model id from the host's model picker. May itself contain `/`.
    pub model: Option<String>,
    /// Restore this durable rpi session when present. If it no longer exists,
    /// the worker starts a fresh session instead of failing the pane.
    pub session_id: Option<String>,
    /// Extra system-prompt text appended after the base prompt. The host uses
    /// this to describe capabilities it renders itself, such as UI cards.
    pub append_system_prompt: Option<String>,
    /// Host metadata hook fired immediately before the first provider request
    /// of a turn. It never observes prompts, credentials or tool payloads.
    pub provider_request_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Metadata-only delta arrival hook (`true` for reasoning), fired before
    /// host-queue admission rather than when the UI eventually polls.
    pub delta_arrival_hook: Option<Arc<dyn Fn(bool) + Send + Sync>>,
    /// Host metadata hook for startup phases (resolve, session build, ready).
    /// With `KATHAMO_STREAM_TIMINGS=1`, also records raw/projected queue
    /// admission (including contention), mutex acquisition, residence, and
    /// embedded projection durations. Backpressure labels count only admissions
    /// that waited for capacity; snapshot-dropped labels are zero-duration counters.
    /// Hooks run outside queue locks. They must remain lightweight.
    pub phase_hook: Option<Arc<dyn Fn(&'static str, std::time::Duration) + Send + Sync>>,
    pub before_tool_call: Option<BeforeToolCall>,
    pub user_input: bool,
}

impl SessionOptions {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            session_dir: None,
            provider: None,
            model: None,
            session_id: None,
            append_system_prompt: None,
            provider_request_hook: None,
            delta_arrival_hook: None,
            phase_hook: None,
            before_tool_call: None,
            user_input: false,
        }
    }

    /// Parse the `provider/model` selector Kathamo uses on launch.
    pub fn with_selector(mut self, selector: Option<&str>) -> Self {
        if let Some(selector) = selector {
            match selector.split_once('/') {
                Some((provider, model)) => {
                    self.provider = Some(provider.to_string());
                    self.model = Some(model.to_string());
                }
                None => self.model = Some(selector.to_string()),
            }
        }
        self
    }

    pub fn with_session(mut self, session_id: Option<&str>) -> Self {
        self.session_id = session_id.map(str::to_owned);
        self
    }

    pub fn with_session_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.session_dir = Some(dir.into());
        self
    }

    pub fn with_append_system_prompt(mut self, prompt: &str) -> Self {
        self.append_system_prompt = Some(prompt.to_owned());
        self
    }

    pub fn with_provider_request_hook(mut self, hook: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.provider_request_hook = Some(hook);
        self
    }

    pub fn with_delta_arrival_hook(mut self, hook: Arc<dyn Fn(bool) + Send + Sync>) -> Self {
        self.delta_arrival_hook = Some(hook);
        self
    }

    pub fn with_phase_hook(
        mut self,
        hook: Arc<dyn Fn(&'static str, std::time::Duration) + Send + Sync>,
    ) -> Self {
        self.phase_hook = Some(hook);
        self
    }
}

enum Command {
    Prompt {
        text: String,
        fallback: Option<(Option<String>, String)>,
    },
    Steer(String),
    Abort,
    Thinking(rpi_ai::ThinkingLevel),
    Shutdown,
}

const COMMAND_RECORDS: usize = 65;
const COMMAND_BYTES: usize = 32 * 1024 * 1024;

struct CommandCharge {
    bytes: usize,
    total: Arc<AtomicUsize>,
}
impl Drop for CommandCharge {
    fn drop(&mut self) {
        self.total.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}
struct QueuedCommand {
    command: Command,
    _charge: CommandCharge,
}

/// Host wakeup callback shared with the worker and forwarder.
///
/// The host uses this to be notified the moment an event is queued instead of
/// discovering it on the next polling tick. Coalesce before acquiring callback
/// locks, rather than making every delta contend on the host's wake channel.
/// The consumer acknowledges BEFORE draining so a racing enqueue cannot lose
/// its wake. A bounded drain explicitly resignals any remaining backlog.
#[derive(Clone, Default)]
pub struct Wake(Arc<WakeState>);

#[derive(Default)]
struct WakeState {
    callback: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    pending: AtomicBool,
}

impl Wake {
    pub fn set(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(mut slot) = self.0.callback.lock() {
            *slot = Some(callback);
        }
        self.acknowledge();
    }

    pub fn signal(&self) {
        if self.0.pending.swap(true, Ordering::AcqRel) {
            return;
        }
        let callback = self.0.callback.lock().ok().and_then(|slot| slot.clone());
        if let Some(callback) = callback {
            callback();
        }
    }

    fn acknowledge(&self) {
        self.0.pending.store(false, Ordering::Release);
    }
}

/// One in-process agent conversation.
pub struct EmbeddedSession {
    events: Arc<EventQueue<SessionEvent>>,
    commands: tokio::sync::mpsc::Sender<QueuedCommand>,
    command_bytes: Arc<AtomicUsize>,
    admission: Mutex<()>,
    shutting_down: Arc<AtomicBool>,
    session_id: Arc<Mutex<Option<String>>>,
    wake: Wake,
    closed: bool,
}

impl EmbeddedSession {
    /// Start the session worker. This returns immediately; model resolution and
    /// harness construction happen on the worker, and prompts sent meanwhile are
    /// queued.
    pub fn spawn(options: SessionOptions) -> Result<Self, String> {
        let event_rx = Arc::new(EventQueue::timed(options.phase_hook.as_ref(), false));
        let event_tx = Arc::clone(&event_rx);
        let (command_tx, command_rx) = tokio::sync::mpsc::channel(COMMAND_RECORDS);
        let shutting_down = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutting_down);
        let session_id = Arc::new(Mutex::new(None));
        let worker_session_id = Arc::clone(&session_id);
        let wake = Wake::default();
        let worker_wake = wake.clone();
        std::thread::Builder::new()
            .name("rpi-embedded".into())
            .spawn(move || {
                run_worker(
                    options,
                    command_rx,
                    event_tx,
                    worker_session_id,
                    worker_shutdown,
                    worker_wake,
                );
            })
            .map_err(|error| format!("Could not start the embedded agent: {error}"))?;
        Ok(Self {
            events: event_rx,
            commands: command_tx,
            command_bytes: Arc::new(AtomicUsize::new(0)),
            admission: Mutex::new(()),
            shutting_down,
            session_id,
            wake,
            closed: false,
        })
    }

    /// Install the host's wakeup callback. The first queued event wakes
    /// immediately; further signals coalesce until the next bounded `poll`.
    pub fn set_wake(&mut self, callback: Arc<dyn Fn() + Send + Sync>) {
        self.wake.set(callback);
        self.wake.signal();
    }

    /// The durable session id, once the harness has opened or created one.
    pub fn session_id(&self) -> Option<String> {
        self.session_id
            .lock()
            .ok()
            .and_then(|session| session.clone())
    }

    pub fn send_prompt(&mut self, prompt: &str) -> Result<(), String> {
        self.send_prompt_with_context(prompt, None, None)
    }

    /// Supply prior context for a fresh session or a missing durable session.
    pub fn send_prompt_with_context(
        &mut self,
        prompt: &str,
        resume: Option<&str>,
        context: Option<String>,
    ) -> Result<(), String> {
        if self.closed {
            return Err("The agent session has closed".into());
        }
        let bytes = prompt
            .len()
            .saturating_add(context.as_ref().map_or(0, String::len));
        self.enqueue_command(bytes, false, || Command::Prompt {
            text: prompt.to_owned(),
            fallback: context.map(|context| (resume.map(str::to_owned), context)),
        })
    }

    pub fn steer(&self, text: &str) -> Result<(), String> {
        self.enqueue_command(text.len(), false, || Command::Steer(text.to_owned()))
    }

    pub fn cancel(&mut self) -> Result<(), String> {
        if self.closed {
            return Err("The agent session has closed".into());
        }
        self.enqueue_command(0, true, || Command::Abort)
    }

    pub fn set_thinking_level(&self, level: &str) -> Result<(), String> {
        let level = match level {
            "inherit" | "auto" => rpi_ai::ThinkingLevel::Medium,
            other => serde_json::from_value(serde_json::Value::String(other.to_owned()))
                .map_err(|_| format!("Unsupported thinking level: {other}"))?,
        };
        self.enqueue_command(0, false, || Command::Thinking(level))
    }

    fn enqueue_command(
        &self,
        bytes: usize,
        abort: bool,
        make: impl FnOnce() -> Command,
    ) -> Result<(), String> {
        let _admission = self.admission.lock().unwrap_or_else(|e| e.into_inner());
        if self.commands.is_closed() {
            return Err("The agent session has closed".into());
        }
        // Reserve one abort slot and one shutdown slot. A full reserved tail
        // already contains an abort after every admitted ordinary command.
        if self.commands.capacity() <= if abort { 1 } else { 2 } {
            return if abort {
                Ok(())
            } else {
                Err("The agent input queue is full".into())
            };
        }
        let mut queued = self.command_bytes.load(Ordering::Relaxed);
        loop {
            let total = queued
                .checked_add(bytes)
                .filter(|total| *total <= COMMAND_BYTES)
                .ok_or_else(|| "The agent input queue is full".to_string())?;
            match self.command_bytes.compare_exchange_weak(
                queued,
                total,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(current) => queued = current,
            }
        }
        let _charge = CommandCharge {
            bytes,
            total: Arc::clone(&self.command_bytes),
        };
        self.commands
            .try_send(QueuedCommand {
                command: make(),
                _charge,
            })
            .map_err(|_| "The agent session has closed".to_string())
    }

    /// Drain a bounded batch, rescheduling the host if more remains.
    pub fn poll(&mut self) -> Vec<SessionEvent> {
        self.wake.acknowledge();
        let (events, more) = self.events.drain();
        if events
            .iter()
            .any(|event| matches!(event, SessionEvent::Closed))
        {
            self.closed = true;
        }
        if more {
            self.wake.signal();
        }
        events
    }
}

impl Drop for EmbeddedSession {
    fn drop(&mut self) {
        // The worker owns its runtime and handles teardown off the host thread.
        self.shutting_down.store(true, Ordering::Release);
        let _ = self.commands.try_send(QueuedCommand {
            command: Command::Shutdown,
            _charge: CommandCharge {
                bytes: 0,
                total: Arc::clone(&self.command_bytes),
            },
        });
        self.events.close();
    }
}

fn run_worker(
    options: SessionOptions,
    commands: tokio::sync::mpsc::Receiver<QueuedCommand>,
    events: Arc<EventQueue<SessionEvent>>,
    session_id: Arc<Mutex<Option<String>>>,
    shutting_down: Arc<AtomicBool>,
    wake: Wake,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        // Each conversation owns a runtime. The provider bridge requires
        // block_in_place, but an idle pane does not need a pool sized to the host.
        // Preserve per-session isolation: sharing would couple block_in_place
        // and teardown across conversations. Consolidation needs independent
        // RSS/latency evidence, not a speculative streaming-path rewrite.
        .worker_threads(2)
        .enable_all()
        .thread_name("rpi-embedded-rt")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            for event in [
                SessionEvent::Error(format!(
                    "Could not start the embedded agent runtime: {error}"
                )),
                SessionEvent::Done,
                SessionEvent::Closed,
            ] {
                if events.send_blocking(event).is_err() {
                    break;
                }
                wake.signal();
            }
            return;
        }
    };
    runtime.block_on(run_session(
        options,
        commands,
        events,
        session_id,
        shutting_down,
        wake,
    ));
}

async fn run_session(
    options: SessionOptions,
    mut commands: tokio::sync::mpsc::Receiver<QueuedCommand>,
    events: Arc<EventQueue<SessionEvent>>,
    session_id: Arc<Mutex<Option<String>>>,
    shutting_down: Arc<AtomicBool>,
    wake: Wake,
) {
    let SessionOptions {
        cwd,
        session_dir,
        provider,
        model,
        session_id: resume,
        append_system_prompt,
        provider_request_hook,
        delta_arrival_hook,
        phase_hook,
        before_tool_call,
        user_input,
    } = options;
    let trusted = project_trusted(&cwd);
    let started = std::time::Instant::now();

    // Resolution reads host-owned files and environment; keep it off the async
    // workers that also serve tool I/O.
    let resolved_started = std::time::Instant::now();
    let resolve_cwd = cwd.clone();
    let resolve_provider = provider.clone();
    let resolve_model = model.clone();
    let resolved = match tokio::task::spawn_blocking(move || {
        if resolve_provider.as_deref() == Some(ANTIGRAVITY) {
            return antigravity::resolve(resolve_model.as_deref());
        }
        rpi_cli::provider::resolve_for_cwd(
            resolve_provider.as_deref(),
            resolve_model.as_deref(),
            None,
            None,
            None,
            &resolve_cwd,
            trusted,
        )
        .map_err(|error| error.to_string())
    })
    .await
    {
        Ok(Ok(resolved)) => resolved,
        Ok(Err(error)) => {
            send_fatal(&events, &wake, error.to_string()).await;
            return;
        }
        Err(error) => {
            send_fatal(
                &events,
                &wake,
                format!("Could not resolve the agent model: {error}"),
            )
            .await;
            return;
        }
    };

    if let Some(hook) = &phase_hook {
        hook("engine.resolve", resolved_started.elapsed());
    }

    let mut args = Args::default();
    args.mode = Mode::Rpc;
    // Several embedded conversations share one process. rpi's extension layer
    // publishes a process-global session id, so keep the embedded core on the
    // built-in tool set and its provider runtimes. Login still loads extensions
    // when it needs an OAuth provider.
    args.no_extensions = true;
    args.offline = true;
    args.host_task_list = true;
    args.host_user_input = user_input;
    args.provider = provider.clone();
    args.model = model.clone();
    args.session = resume.clone();
    args.session_dir = session_dir;
    if let Some(prompt) = append_system_prompt {
        args.append_system_prompt.push(prompt);
    }

    let build_started = std::time::Instant::now();
    let mut built = rpi_cli::session::build_with_tool_hook(
        &resolved,
        &args,
        &cwd,
        trusted,
        before_tool_call.clone(),
    )
    .await;
    if built.is_err() && resume.is_some() {
        // A stale id must not brick the pane; the host transcript is still on
        // screen, so start a fresh durable session and publish its new id.
        let mut fresh = args.clone();
        fresh.session = None;
        if let Ok(retry) = rpi_cli::session::build_with_tool_hook(
            &resolved,
            &fresh,
            &cwd,
            trusted,
            before_tool_call,
        )
        .await
        {
            built = Ok(retry);
        }
    }

    let (harness, broadcast_rx, reload) = match built {
        Ok(built) => built,
        Err(error) => {
            send_fatal(&events, &wake, describe_build_error(error)).await;
            return;
        }
    };
    let harness = Arc::new(harness);
    // Embedded disables extensions. Replace only the unused CLI observer;
    // per-run durable/settling wrappers still wrap this host emitter.
    drop(broadcast_rx);
    let pending = Arc::new(EventQueue::timed(phase_hook.as_ref(), true));
    let emitter = Arc::new(HostEmitter {
        pending: Arc::clone(&pending),
        delta_arrival: delta_arrival_hook,
    });
    if let Err(error) = harness.set_agent_emitter(Some(emitter)).await {
        send_fatal(
            &events,
            &wake,
            format!("Could not install host event delivery: {error}"),
        )
        .await;
        return;
    }
    if let Some(hook) = &phase_hook {
        hook("engine.session_build", build_started.elapsed());
        hook("engine.session_ready", started.elapsed());
    }
    if let Ok(mut slot) = session_id.lock() {
        *slot = Some(harness.session_id());
    }
    // The host uses this only for metadata timing. Embedded sessions run without
    // extensions, so no extension provider hooks are displaced.
    if let Some(hook) = provider_request_hook {
        struct HostTimingHooks(Arc<dyn Fn() + Send + Sync>);
        impl rpi_ai::ProviderHooks for HostTimingHooks {
            fn before_request(
                &self,
                _model: &rpi_ai::Model,
                _ctx: &rpi_ai::types::Context,
                _opts: &rpi_ai::SimpleStreamOptions,
            ) -> Option<rpi_ai::SimpleStreamOptionsPatch> {
                (self.0)();
                None
            }
        }
        let hooks: Arc<dyn rpi_ai::ProviderHooks> = Arc::new(HostTimingHooks(hook));
        if let Err(error) = harness.set_provider_hooks(Some(hooks)).await {
            let _ = events
                .send(SessionEvent::Status(format!(
                    "Could not install timing hooks: {error}"
                )))
                .await;
        }
    }

    // Reliable raw delivery is bounded independently from projected host events.
    // Each queue admits one oversized event exclusively; effective retained
    // payload is at most the two budgets plus the largest in-flight records.
    let forward_tx = events.clone();
    let forward_wake = wake.clone();
    let forward_pending = Arc::clone(&pending);
    let running = Arc::new(AtomicBool::new(false));
    let forward_running = Arc::clone(&running);
    let (finished, mut completions) = tokio::sync::mpsc::channel::<Option<String>>(1);
    let forward_timing = phase_hook
        .clone()
        .filter(|_| std::env::var_os("KATHAMO_STREAM_TIMINGS").is_some_and(|value| value != "0"));
    let forwarder = tokio::spawn(async move {
        let _reload = reload;
        loop {
            tokio::select! {
                // Completion follows all async emissions; admitted records
                // must reach the UI before Done.
                biased;
                Some(event) = forward_pending.recv() => {
                    let started = forward_timing.as_ref().map(|_| std::time::Instant::now());
                    let event = event.into_session();
                    if let (Some(hook), Some(started)) = (&forward_timing, started) {
                        hook("bridge.embedded.project", started.elapsed());
                    }
                    if forward_tx.send(event).await.is_err() {
                        forward_pending.close();
                        break;
                    }
                    forward_wake.signal();
                },
                Some(error) = completions.recv() => {
                    if let Some(error) = error {
                        let _ = forward_tx.send(SessionEvent::Error(error)).await;
                    }
                    forward_running.store(false, Ordering::SeqCst);
                    let _ = forward_tx.send(SessionEvent::Done).await;
                    forward_wake.signal();
                }
            }
        }
    });
    wake.signal(); // The durable session id is now available to the host.

    let mut prompt_task = None;
    while let Some(command) = commands.recv().await {
        let QueuedCommand { command, _charge } = command;
        if shutting_down.load(Ordering::Acquire) {
            break;
        }
        match command {
            Command::Prompt { mut text, fallback } => {
                if running.swap(true, Ordering::SeqCst) {
                    let _ = events
                        .send(SessionEvent::Error(
                            "The agent is already running a prompt".into(),
                        ))
                        .await;
                    wake.signal();
                    continue;
                }
                if let Some((expected, context)) = fallback {
                    if expected.as_deref() != Some(harness.session_id().as_str()) {
                        text = context;
                    }
                }
                let lane: Arc<dyn AgentLane> = harness.clone();
                let finished = finished.clone();
                prompt_task = Some(tokio::spawn(async move {
                    let error = lane.prompt_text(&text, Vec::new()).await.err();
                    let _ = finished.send(error.map(|error| error.to_string())).await;
                }));
            }
            Command::Steer(text) => {
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default().as_millis() as i64;
                let message = AgentMessage::User(rpi_ai::types::UserMessage::new(text, timestamp));
                if let Err(error) = harness.steer(message).await {
                    let _ = events.send(SessionEvent::Error(error.to_string())).await;
                    wake.signal();
                }
            }
            Command::Abort => {
                if running.load(Ordering::SeqCst) {
                    harness.request_stop_round();
                    if let Err(error) = harness.abort_reclaiming(true).await {
                        let _ = events.send(SessionEvent::Error(error.to_string())).await;
                        wake.signal();
                    }
                }
            }
            Command::Thinking(level) => {
                if let Err(error) = harness.set_thinking_level(level).await {
                    let _ = events.send(SessionEvent::Error(error.to_string())).await;
                    wake.signal();
                }
            }
            Command::Shutdown => break,
        }
    }

    pending.close();
    harness.request_stop_round();
    let _ = harness.abort_reclaiming(true).await;
    if let Some(task) = prompt_task {
        let _ = task.await;
    }
    forwarder.abort();
    let _ = events.send(SessionEvent::Closed).await;
    wake.signal();
}

fn project_trusted(cwd: &Path) -> bool {
    rpi_cli::config::project_trust_decision(cwd)
        .ok()
        .flatten()
        .unwrap_or(true)
}

fn describe_build_error(error: BuildError) -> String {
    match error {
        BuildError::SessionNotFound { requested, dir } => format!(
            "The saved agent session {requested} was not found in {dir}. Start a new conversation."
        ),
        other => format!("Could not start the embedded agent: {other}"),
    }
}

async fn send_fatal(events: &EventQueue<SessionEvent>, wake: &Wake, message: String) {
    for event in [
        SessionEvent::Error(message),
        SessionEvent::Done,
        SessionEvent::Closed,
    ] {
        if events.send(event).await.is_err() {
            break;
        }
        wake.signal();
    }
}

fn finalized_usage(event: &RpiEvent) -> Option<AgentUsage> {
    let RpiEvent::MessageEnd {
        message: AgentMessage::Assistant(assistant),
        ..
    } = event
    else {
        return None;
    };
    let usage = &assistant.usage;
    Some(AgentUsage {
        input: usage.input,
        output: usage.output,
        cache_read: usage.cache_read,
        cache_write: usage.cache_write,
        cost: (usage.cost.total.is_finite() && usage.cost.total > 0.0).then_some(usage.cost.total),
    })
}

fn pending_event(event: RpiEvent) -> Option<PendingEvent> {
    let event = match event {
        RpiEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => match assistant_message_event {
            AssistantMessageEvent::TextDelta { delta, .. } => SessionEvent::Text(delta),
            AssistantMessageEvent::ThinkingDelta { delta, .. } => SessionEvent::Thinking(delta),
            _ => return None,
        },
        RpiEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            if let Some(hook) = TOOL_ARRIVAL_HOOK.get() {
                hook(&tool_call_id);
            }
            SessionEvent::ToolStart {
                id: Some(tool_call_id),
                name: tool_name,
                input: serde_json::to_string(&args).unwrap_or_default(),
            }
        }
        RpiEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            partial_result,
            ..
        } => {
            tool_output_arrived(&tool_call_id, &partial_result);
            let bytes = tool_call_id.len() + tool_name.len() + result_bytes(&partial_result);
            return Some(PendingEvent::Update {
                id: tool_call_id,
                name: tool_name,
                result: partial_result,
                bytes,
            });
        }
        RpiEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
        } => {
            if let Some(hook) = TOOL_END_HOOK.get() {
                hook(&tool_call_id);
            }
            tool_output_arrived(&tool_call_id, &result);
            let bytes = tool_call_id.len() + tool_name.len() + result_bytes(&result);
            return Some(PendingEvent::End {
                id: tool_call_id,
                name: tool_name,
                result,
                failed: is_error,
                bytes,
            });
        }
        RpiEvent::MessageEnd {
            message: AgentMessage::Assistant(assistant),
            ..
        } if assistant.stop_reason == StopReason::Error => SessionEvent::Error(
            assistant
                .error_message
                .unwrap_or_else(|| "The agent encountered an error".into()),
        ),
        RpiEvent::RetryScheduled {
            attempt,
            max_retries,
            delay_ms,
            error,
        } => SessionEvent::Status(format!(
            "Retrying model request (attempt {attempt}/{max_retries}; delay {delay_ms} ms): {error}"
        )),
        _ => return None,
    };
    Some(PendingEvent::Ready(event))
}

fn tool_output_arrived(id: &str, result: &rpi_agent::AgentToolResult) {
    if let Some(hook) = TOOL_OUTPUT_HOOK.get() {
        // The rendered snapshot joins text blocks with newlines. Check without
        // projecting/copying the cumulative tool result on the producer path.
        let mut blocks = 0usize;
        for content in &result.content {
            if let rpi_agent::TextContentOrImage::Text(text) = content {
                blocks += 1;
                if !text.text.is_empty() || blocks > 1 {
                    hook(id);
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
async fn forward_event(event: RpiEvent, events: &EventQueue<SessionEvent>) -> bool {
    let sent_usage = if let Some(usage) = finalized_usage(&event) {
        if events.send(SessionEvent::Usage(usage)).await.is_err() {
            return false;
        }
        true
    } else {
        false
    };
    match pending_event(event) {
        Some(event) => events.send(event.into_session()).await.is_ok(),
        None => sent_usage,
    }
}

fn tool_result_text(result: &rpi_agent::AgentToolResult) -> String {
    let (bytes, count) = result
        .content
        .iter()
        .fold((0usize, 0usize), |(bytes, count), content| match content {
            rpi_agent::TextContentOrImage::Text(text) => (bytes + text.text.len(), count + 1),
            rpi_agent::TextContentOrImage::Image(_) => (bytes, count),
        });
    let mut out = String::with_capacity(bytes + count.saturating_sub(1));
    let mut first = true;
    for content in &result.content {
        if let rpi_agent::TextContentOrImage::Text(text) = content {
            if !first {
                out.push('\n');
            }
            first = false;
            out.push_str(&text.text);
        }
    }
    out
}

struct EnvironmentValues;

impl ValueSource for EnvironmentValues {
    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// Result of one in-process authentication step.
#[derive(Clone, Debug)]
pub enum LoginEvent {
    BrowserUrl(String),
    Output(String),
    Finished(Result<(), String>),
}

/// In-process replacement for `rpi auth login`.
///
/// API keys and native browser/device grants use the CLI's atomic `auth.json`
/// writer and the inference runtime's renewal binding. [`Login::start`] prefers
/// sign-in when available; [`Login::start_with_method`] explicitly selects a
/// method. Extension-owned `begin`/`exchange` actions remain supported.
/// Authorization values are never echoed to the host event stream.
pub struct Login {
    events: mpsc::Receiver<LoginEvent>,
    input: Option<mpsc::Sender<String>>,
    finished: bool,
}

impl Login {
    pub fn start(provider: &str, cwd: &Path) -> Result<Self, String> {
        Self::start_selected(provider, cwd, None)
    }

    /// Explicitly choose API-key, browser OAuth, or supported headless device OAuth.
    pub fn start_with_method(
        provider: &str,
        cwd: &Path,
        method: AuthMethod,
    ) -> Result<Self, String> {
        let methods = auth_methods_for_cwd(provider, cwd);
        match method {
            AuthMethod::ApiKey if !methods.api_key => {
                return Err("This provider does not support API-key sign-in".into())
            }
            AuthMethod::OAuth if !methods.oauth => {
                return Err("This provider does not support OAuth sign-in".into())
            }
            AuthMethod::DeviceCode
                if !matches!(
                    provider,
                    "openai-codex"
                        | "openai-codex-device"
                        | "github-copilot"
                        | "kimi-code"
                        | "factory-droid"
                        | "muse-code"
                        | "xai-oauth"
                ) =>
            {
                return Err("This provider does not support device-code sign-in".into())
            }
            _ => {}
        }
        Self::start_selected(provider, cwd, Some(method))
    }

    fn start_selected(
        provider: &str,
        cwd: &Path,
        method: Option<AuthMethod>,
    ) -> Result<Self, String> {
        let (event_tx, event_rx) = mpsc::channel();
        let (input_tx, input_rx) = mpsc::channel();
        let provider = provider.to_string();
        let cwd = cwd.to_path_buf();
        std::thread::Builder::new()
            .name("rpi-embedded-login".into())
            .spawn(move || login_worker(provider, cwd, method, event_tx, input_rx))
            .map_err(|error| format!("Could not start in-process authentication: {error}"))?;
        Ok(Self {
            events: event_rx,
            input: Some(input_tx),
            finished: false,
        })
    }

    pub fn try_event(&mut self) -> Option<LoginEvent> {
        let event = self.events.try_recv().ok()?;
        if matches!(event, LoginEvent::Finished(_)) {
            self.finished = true;
            self.input = None;
        }
        Some(event)
    }

    pub fn submit(&mut self, input: &str) -> Result<(), String> {
        if self.finished {
            return Err("Authentication has already finished".into());
        }
        if input.contains(['\r', '\n', '\0']) {
            return Err("Authentication input must be a single line".into());
        }
        self.input
            .as_ref()
            .ok_or_else(|| "Authentication is not waiting for input".to_string())?
            .send(input.to_string())
            .map_err(|_| "Authentication has closed".to_string())
    }

    pub fn cancel(&mut self) {
        self.finished = true;
        self.input = None;
    }
}

impl Drop for Login {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn login_worker(
    provider: String,
    cwd: PathBuf,
    method: Option<AuthMethod>,
    events: mpsc::Sender<LoginEvent>,
    inputs: mpsc::Receiver<String>,
) {
    if method == Some(AuthMethod::ApiKey) {
        run_api_key(provider, events, inputs);
        return;
    }
    if provider == ANTIGRAVITY {
        let result = antigravity::login(&events, &inputs);
        let _ = events.send(LoginEvent::Finished(result));
        return;
    }
    let runtime = match auth_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = events.send(LoginEvent::Finished(Err(error)));
            return;
        }
    };
    let _runtime = runtime.enter();
    let trusted = project_trusted(&cwd);
    let prepared = rpi_cli::session::prepare_extensions(&Args::default(), &cwd, trusted).ok();
    let snapshot = prepared
        .as_ref()
        .and_then(|prepared| prepared.extension_session.snapshot_arc());
    let oauth = snapshot
        .as_ref()
        .filter(|snapshot| snapshot.oauth_provider(&provider).is_some());

    if let Some(snapshot) = oauth {
        run_oauth(provider, Arc::clone(snapshot), events, inputs);
    } else if rpi_cli::native_auth::supports(&provider) {
        let emit = |event| {
            let event = match event {
                rpi_cli::native_auth::Event::BrowserUrl(url) => LoginEvent::BrowserUrl(url),
                rpi_cli::native_auth::Event::Output(text) => LoginEvent::Output(text),
            };
            let _ = events.send(event);
        };
        let result = rpi_cli::native_auth::login(
            &provider,
            &emit,
            &inputs,
            method == Some(AuthMethod::DeviceCode),
        );
        let _ = events.send(LoginEvent::Finished(result));
    } else if method.is_some() {
        let _ = events.send(LoginEvent::Finished(Err(
            "This provider has no OAuth sign-in implementation".into(),
        )));
    } else {
        run_api_key(provider, events, inputs);
    }
}

fn run_api_key(provider: String, events: mpsc::Sender<LoginEvent>, inputs: mpsc::Receiver<String>) {
    let _ = events.send(LoginEvent::Output(format!(
        "Enter API key for {provider}: "
    )));
    let key = match inputs.recv() {
        Ok(key) => key,
        Err(_) => {
            let _ = events.send(LoginEvent::Finished(Err(
                "Authentication was cancelled".into()
            )));
            return;
        }
    };
    let _ = events.send(LoginEvent::Finished(save_api_key(&provider, &key)));
}

pub fn save_api_key(provider: &str, key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() || key.contains(['\r', '\n', '\0']) {
        return Err("Enter a nonempty API key on one line.".into());
    }
    let credential = Credential::ApiKey {
        key: Some(key.to_string()),
        env: None,
    };
    rpi_cli::config::upsert_credential(provider, credential)
        .map_err(|error| format!("Could not save credentials: {error}"))
}

pub fn disconnect_provider(provider: &str) -> Result<(), String> {
    if provider == ANTIGRAVITY {
        return antigravity::disconnect();
    }
    rpi_cli::config::delete_credential(provider)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn run_oauth(
    provider: String,
    snapshot: Arc<rpi_extensions::RegistrySnapshot>,
    events: mpsc::Sender<LoginEvent>,
    inputs: mpsc::Receiver<String>,
) {
    let mut input = serde_json::json!({});
    let mut action = "begin";
    loop {
        let raw = match rpi_extensions::request_oauth(&snapshot, &provider, action, input) {
            Ok(value) => value,
            Err(error) => {
                let _ = events.send(LoginEvent::Finished(Err(format!(
                    "OAuth {action} failed: {error}"
                ))));
                return;
            }
        };
        let response = match rpi_extensions::OAuthActionResponse::parse(raw) {
            Ok(response) => response,
            Err(error) => {
                let _ = events.send(LoginEvent::Finished(Err(format!(
                    "Invalid OAuth response: {error}"
                ))));
                return;
            }
        };
        match response.kind {
            rpi_extensions::OAuthResponseKind::Credential => {
                let token = match response.credential() {
                    Ok(Some(token)) => token,
                    _ => {
                        let _ = events.send(LoginEvent::Finished(Err(
                            "OAuth response contained no valid credential".into(),
                        )));
                        return;
                    }
                };
                let result = rpi_cli::config::upsert_oauth_token(
                    &provider,
                    token.access,
                    token.refresh,
                    token.expires_at,
                )
                .map_err(|error| format!("Could not save OAuth credentials: {error}"));
                let _ = events.send(LoginEvent::Finished(result));
                return;
            }
            rpi_extensions::OAuthResponseKind::Error => {
                let _ = events.send(LoginEvent::Finished(Err(
                    "The OAuth provider rejected the request".into(),
                )));
                return;
            }
            rpi_extensions::OAuthResponseKind::Success => {
                let _ = events.send(LoginEvent::Finished(Err(
                    "OAuth login completed without credentials".into(),
                )));
                return;
            }
            rpi_extensions::OAuthResponseKind::Interaction => {
                let display = response.payload.get("display").unwrap_or(&response.payload);
                let mut text = String::new();
                if let Some(title) = display.get("title").and_then(serde_json::Value::as_str) {
                    text.push_str(title);
                    text.push('\n');
                }
                if let Some(url) = display.get("url").and_then(serde_json::Value::as_str) {
                    let _ = events.send(LoginEvent::BrowserUrl(url.to_owned()));
                    text.push_str(url);
                    text.push('\n');
                }
                if let Some(instructions) = display
                    .get("instructions")
                    .and_then(serde_json::Value::as_array)
                {
                    for instruction in instructions.iter().filter_map(serde_json::Value::as_str) {
                        text.push_str(instruction);
                        text.push('\n');
                    }
                }
                text.push_str("OAuth input: ");
                let _ = events.send(LoginEvent::Output(text));
                let value = match inputs.recv() {
                    Ok(value) => value,
                    Err(_) => {
                        let _ = events.send(LoginEvent::Finished(Err(
                            "Authentication was cancelled".into(),
                        )));
                        return;
                    }
                };
                if value.trim().is_empty() {
                    let _ = events.send(LoginEvent::Finished(Err(
                        "Empty OAuth input; nothing was saved.".into(),
                    )));
                    return;
                }
                input = serde_json::json!({ "value": value.trim() });
                action = "exchange";
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finalized_usage_is_emitted_once_with_partial_failures_and_unknown_prices() {
        let pending = Arc::new(EventQueue::default());
        let emitter = HostEmitter {
            pending: Arc::clone(&pending),
            delta_arrival: None,
        };
        for (stop_reason, cost) in [
            (StopReason::Stop, 0.25),
            (StopReason::Error, 0.0),
            (StopReason::Aborted, 0.0),
        ] {
            let mut assistant = rpi_ai::AssistantMessage::empty(rpi_ai::Api::Faux, "faux", "m", 0);
            assistant.usage.input = 9_007_199_254_740_993;
            assistant.usage.output = 12;
            assistant.usage.cache_read = 34;
            assistant.usage.cache_write = 56;
            assistant.usage.cost.total = cost;
            assistant.stop_reason = stop_reason;
            assistant.error_message = Some("provider failed".into());
            let partial = Arc::new(assistant.clone());
            rpi_agent::AgentEmitter::emit(
                &emitter,
                RpiEvent::MessageUpdate {
                    message_id: None,
                    message: Arc::clone(&partial),
                    assistant_message_event: AssistantMessageEvent::TextDelta {
                        content_index: 0,
                        delta: "partial".into(),
                        partial,
                    },
                },
            )
            .await;
            rpi_agent::AgentEmitter::emit(
                &emitter,
                RpiEvent::MessageEnd {
                    message: AgentMessage::Assistant(Box::new(assistant)),
                    message_id: None,
                },
            )
            .await;
            let (raw, more) = pending.drain();
            assert!(!more);
            let events: Vec<_> = raw.into_iter().map(PendingEvent::into_session).collect();
            assert_eq!(events[0], SessionEvent::Text("partial".into()));
            assert_eq!(
                events[1],
                SessionEvent::Usage(AgentUsage {
                    input: 9_007_199_254_740_993,
                    output: 12,
                    cache_read: 34,
                    cache_write: 56,
                    cost: (cost > 0.0).then_some(cost),
                })
            );
            if stop_reason == StopReason::Error {
                assert_eq!(events[2], SessionEvent::Error("provider failed".into()));
                assert_eq!(events.len(), 3);
            } else {
                assert_eq!(events.len(), 2);
            }
        }
    }

    fn test_session(
        events: Arc<EventQueue<SessionEvent>>,
    ) -> (EmbeddedSession, tokio::sync::mpsc::Receiver<QueuedCommand>) {
        let (commands, receiver) = tokio::sync::mpsc::channel(COMMAND_RECORDS);
        (
            EmbeddedSession {
                events,
                commands,
                command_bytes: Arc::new(AtomicUsize::new(0)),
                admission: Mutex::new(()),
                shutting_down: Arc::new(AtomicBool::new(false)),
                session_id: Arc::new(Mutex::new(None)),
                wake: Wake::default(),
                closed: false,
            },
            receiver,
        )
    }

    #[test]
    fn wake_coalesces_bursts_and_rearms_after_poll() {
        let (mut session, _commands) = test_session(Arc::new(EventQueue::default()));
        let wakes = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&wakes);
        session.wake.set(Arc::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
        }));
        for _ in 0..10_000 {
            session.wake.signal();
        }
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert!(session.poll().is_empty());
        session.wake.signal();
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn wake_installation_and_replacement_rearm_preexisting_events() {
        let queue = Arc::new(EventQueue::default());
        let (mut session, _commands) = test_session(Arc::clone(&queue));
        assert!(queue
            .try_send(SessionEvent::Text("before callback".into()))
            .is_ok());
        session.wake.signal(); // No callback yet; leaves a pending signal.
        let first = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&first);
        session.set_wake(Arc::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
        }));
        assert_eq!(first.load(Ordering::Relaxed), 1);
        session.wake.signal();
        assert_eq!(first.load(Ordering::Relaxed), 1);
        let replacement = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&replacement);
        session.set_wake(Arc::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
        }));
        assert_eq!(replacement.load(Ordering::Relaxed), 1);
        assert_eq!(
            session.poll(),
            vec![SessionEvent::Text("before callback".into())]
        );
        session.wake.signal();
        assert_eq!(replacement.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn wake_callback_can_poll_and_reenter_without_holding_callback_lock() {
        let queue = Arc::new(EventQueue::default());
        let (session, _commands) = test_session(Arc::clone(&queue));
        let session = Arc::new(Mutex::new(session));
        let weak = Arc::downgrade(&session);
        let wakes = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&wakes);
        let received = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&received);
        let callback = Arc::new(move || {
            let index = count.fetch_add(1, Ordering::Relaxed);
            let session = weak.upgrade().unwrap();
            let wake = {
                let mut session = session.lock().unwrap();
                output.lock().unwrap().extend(session.poll());
                session.wake.clone()
            };
            if index == 0 {
                wake.signal();
            }
        });
        let wake = {
            let session = session.lock().unwrap();
            session.wake.set(callback);
            session.wake.clone()
        };
        assert!(queue.try_send(SessionEvent::Text("first".into())).is_ok());
        wake.signal();
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
        assert!(queue.try_send(SessionEvent::Text("second".into())).is_ok());
        wake.signal();
        assert_eq!(wakes.load(Ordering::Relaxed), 3);
        assert_eq!(
            *received.lock().unwrap(),
            vec![
                SessionEvent::Text("first".into()),
                SessionEvent::Text("second".into()),
            ]
        );
    }

    #[test]
    fn enqueue_racing_poll_is_delivered_or_wakes_consumer() {
        let queue = Arc::new(EventQueue::default());
        let (mut session, _commands) = test_session(Arc::clone(&queue));
        let wakes = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&wakes);
        session.wake.set(Arc::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
        }));
        let boundary = Arc::new(std::sync::Barrier::new(2));
        let producer_boundary = Arc::clone(&boundary);
        let wake = session.wake.clone();
        let producer = std::thread::spawn(move || {
            for _ in 0..2_000 {
                producer_boundary.wait();
                assert!(queue.try_send(SessionEvent::Text("delta".into())).is_ok());
                wake.signal();
                producer_boundary.wait();
            }
        });
        for _ in 0..2_000 {
            wakes.store(0, Ordering::Relaxed);
            boundary.wait();
            let events = session.poll();
            boundary.wait();
            if events.is_empty() {
                assert!(
                    wakes.load(Ordering::Relaxed) > 0,
                    "queued delta lost its wake"
                );
                assert_eq!(session.poll(), vec![SessionEvent::Text("delta".into())]);
            } else {
                assert_eq!(events, vec![SessionEvent::Text("delta".into())]);
            }
        }
        producer.join().unwrap();
    }

    #[tokio::test]
    async fn bounded_delivery_yields_resignals_and_retains_terminal_events() {
        let queue = Arc::new(EventQueue::default());
        for _ in 0..EVENT_QUEUE_RECORDS {
            assert!(queue.try_send(SessionEvent::Text("x".into())).is_ok());
        }
        assert!(queue.try_send(SessionEvent::Done).is_err());
        let (mut session, _commands) = test_session(Arc::clone(&queue));
        let wakes = Arc::new(AtomicUsize::new(0));
        let wake_count = Arc::clone(&wakes);
        session.wake.set(Arc::new(move || {
            wake_count.fetch_add(1, Ordering::Relaxed);
        }));
        let pending = Arc::clone(&queue);
        let done = tokio::spawn(async move { pending.send(SessionEvent::Done).await });
        tokio::task::yield_now().await;
        assert!(!done.is_finished(), "backpressure must asynchronously wait");
        assert_eq!(session.poll().len(), POLL_RECORDS);
        assert_eq!(
            wakes.load(Ordering::Relaxed),
            1,
            "backlog must resignal the host"
        );
        done.await.unwrap().unwrap();
        let mut remaining = Vec::new();
        loop {
            let batch = session.poll();
            if batch.is_empty() {
                break;
            }
            remaining.extend(batch);
        }
        assert_eq!(remaining.len(), EVENT_QUEUE_RECORDS - POLL_RECORDS + 1);
        assert!(matches!(remaining.last(), Some(SessionEvent::Done)));
    }

    #[tokio::test]
    async fn byte_budget_and_close_unblock_waiting_producers() {
        let queue = Arc::new(EventQueue::default());
        let chunk = "x".repeat(POLL_BYTES / 2);
        for _ in 0..EVENT_QUEUE_BYTES / chunk.len() {
            assert!(queue.try_send(SessionEvent::Text(chunk.clone())).is_ok());
        }
        assert!(queue
            .try_send(SessionEvent::Text("overflow".into()))
            .is_err());
        let (batch, more) = queue.drain();
        assert_eq!(batch.len(), 2);
        assert!(more);
        let oversized = SessionEvent::ToolEnd {
            id: Some("large".into()),
            name: "bash".into(),
            output: "x".repeat(EVENT_QUEUE_BYTES + 1),
            failed: false,
        };
        let pending = Arc::clone(&queue);
        let blocked = tokio::spawn(async move { pending.send(oversized).await });
        tokio::task::yield_now().await;
        assert!(!blocked.is_finished());
        queue.close();
        assert!(blocked.await.unwrap().is_err());
        let exclusive = EventQueue::default();
        assert!(exclusive
            .try_send(SessionEvent::Text("x".repeat(EVENT_QUEUE_BYTES + 1)))
            .is_ok());
        assert!(exclusive.try_send(SessionEvent::Done).is_err());
        let (events, more) = exclusive.drain();
        assert_eq!(events.len(), 1);
        assert!(!more);
    }

    #[tokio::test]
    async fn reliable_emitter_coalesces_before_projection_and_preserves_boundaries() {
        use rpi_agent::AgentEmitter;
        let pending = Arc::new(EventQueue::default());
        let emitter = HostEmitter {
            pending: Arc::clone(&pending),
            delta_arrival: None,
        };
        emitter
            .emit(RpiEvent::ToolExecutionStart {
                tool_call_id: "a".into(),
                tool_name: "bash".into(),
                args: serde_json::json!({}),
            })
            .await;
        let update = |text: &str| RpiEvent::ToolExecutionUpdate {
            tool_call_id: "a".into(),
            tool_name: "bash".into(),
            args: serde_json::json!({}),
            partial_result: Arc::new(rpi_agent::AgentToolResult {
                content: vec![rpi_agent::TextContentOrImage::text(text)],
                ..Default::default()
            }),
        };
        for _ in 0..1000 {
            emitter.try_emit(update("old"));
        }
        emitter.try_emit(update("latest"));
        assert_eq!(pending.state.lock().unwrap().events.len(), 2);
        emitter
            .emit(RpiEvent::RetryScheduled {
                attempt: 1,
                max_retries: 2,
                delay_ms: 1,
                error: "boundary".into(),
            })
            .await;
        emitter.try_emit(update("after"));
        emitter
            .emit(RpiEvent::ToolExecutionEnd {
                tool_call_id: "a".into(),
                tool_name: "bash".into(),
                is_error: false,
                result: rpi_agent::AgentToolResult {
                    content: vec![rpi_agent::TextContentOrImage::text("final")],
                    ..Default::default()
                },
            })
            .await;
        let (raw, more) = pending.drain();
        assert!(!more);
        let events: Vec<_> = raw.into_iter().map(PendingEvent::into_session).collect();
        assert!(matches!(&events[..], [
            SessionEvent::ToolStart { .. },
            SessionEvent::ToolUpdate { output: first, .. },
            SessionEvent::Status(_),
            SessionEvent::ToolUpdate { output: second, .. },
            SessionEvent::ToolEnd { output: final_result, failed: false, .. },
        ] if first == "latest" && second == "after" && final_result == "final"));
    }

    #[test]
    fn embedded_commands_bound_bytes_and_reserve_abort_shutdown_slots() {
        let (mut session, mut receiver) = test_session(Arc::new(EventQueue::default()));
        assert!(session.send_prompt(&"x".repeat(COMMAND_BYTES + 1)).is_err());
        for _ in 0..COMMAND_RECORDS - 2 {
            session.set_thinking_level("medium").unwrap();
        }
        assert!(session.send_prompt("full").is_err());
        for _ in 0..1000 {
            session.cancel().unwrap();
        }
        assert_eq!(session.commands.capacity(), 1);
        drop(session);
        let mut count = 0;
        let mut last_shutdown = false;
        while let Ok(command) = receiver.try_recv() {
            count += 1;
            last_shutdown = matches!(command.command, Command::Shutdown);
        }
        assert_eq!(count, COMMAND_RECORDS);
        assert!(last_shutdown);
    }

    #[test]
    fn cached_extension_methods_are_project_scoped() {
        let first = PathBuf::from(format!("/auth-cache-test/{}", uuid::Uuid::new_v4()));
        let second = PathBuf::from(format!("/auth-cache-test/{}", uuid::Uuid::new_v4()));
        let provider = "project-specific-oauth-test";
        EXTENSION_OAUTH_METHODS.write().unwrap().insert(
            first.clone(),
            Arc::new([provider.to_owned()].into_iter().collect()),
        );
        assert!(cached_auth_methods(provider, &first).oauth);
        assert!(!cached_auth_methods(provider, &second).oauth);
        assert!(!auth_methods(provider).oauth);
        EXTENSION_OAUTH_METHODS.write().unwrap().remove(&first);
    }

    #[test]
    fn selector_splits_provider_from_a_model_that_contains_slashes() {
        let options = SessionOptions::new("/tmp").with_selector(Some("commandcode/deepseek/v4"));
        assert_eq!(options.provider.as_deref(), Some("commandcode"));
        assert_eq!(options.model.as_deref(), Some("deepseek/v4"));
    }

    #[test]
    fn tool_text_joins_text_blocks_only() {
        let result = rpi_agent::AgentToolResult {
            content: vec![
                rpi_agent::TextContentOrImage::text("one"),
                rpi_agent::TextContentOrImage::text("two"),
            ],
            details: serde_json::Value::Null,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
        };
        assert_eq!(tool_result_text(&result), "one\ntwo");
    }

    #[test]
    fn environment_value_source_reads_the_process_environment() {
        std::env::set_var("RPI_EMBEDDED_TEST_VALUE", "ok");
        let values = EnvironmentValues;
        assert_eq!(values.env("RPI_EMBEDDED_TEST_VALUE").as_deref(), Some("ok"));
        assert_eq!(values.env("RPI_EMBEDDED_TEST_MISSING"), None);
        std::env::remove_var("RPI_EMBEDDED_TEST_VALUE");
    }

    #[tokio::test]
    async fn event_mapping_covers_text_tools_retries_and_completion() {
        let tx = EventQueue::default();
        let partial = Arc::new(rpi_ai::AssistantMessage::empty(
            rpi_ai::Api::Faux,
            "faux",
            "m",
            0,
        ));
        forward_event(
            RpiEvent::MessageUpdate {
                message_id: None,
                message: Arc::clone(&partial),
                assistant_message_event: AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "hello".into(),
                    partial: Arc::clone(&partial),
                },
            },
            &tx,
        )
        .await;
        forward_event(
            RpiEvent::ToolExecutionStart {
                tool_call_id: "t1".into(),
                tool_name: "bash".into(),
                args: serde_json::json!({"command": "printf hi"}),
            },
            &tx,
        )
        .await;
        let result = rpi_agent::AgentToolResult {
            content: vec![rpi_agent::TextContentOrImage::text("hi")],
            details: serde_json::Value::Null,
            usage: None,
            added_tool_names: Vec::new(),
            terminate: false,
        };
        forward_event(
            RpiEvent::ToolExecutionEnd {
                tool_call_id: "t1".into(),
                tool_name: "bash".into(),
                result,
                is_error: false,
            },
            &tx,
        )
        .await;
        forward_event(
            RpiEvent::RetryScheduled {
                attempt: 1,
                max_retries: 3,
                delay_ms: 250,
                error: "busy".into(),
            },
            &tx,
        )
        .await;
        forward_event(
            RpiEvent::AgentEnd {
                messages: Vec::new(),
            },
            &tx,
        )
        .await;
        let (events, more) = tx.drain();
        assert!(!more);
        assert!(matches!(&events[0], SessionEvent::Text(text) if text == "hello"));
        assert!(
            matches!(&events[1], SessionEvent::ToolStart { id, name, input }
            if id.as_deref() == Some("t1") && name == "bash" && input.contains("printf hi"))
        );
        assert!(
            matches!(&events[2], SessionEvent::ToolEnd { output, failed, .. }
            if output == "hi" && !failed)
        );
        assert!(matches!(&events[3], SessionEvent::Status(text) if text.contains("attempt 1/3")));
        assert_eq!(events.len(), 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_faux_harness_streams_text_and_completion_through_the_mapping() {
        use rpi_ai::providers::faux::{FauxProvider, FauxScript};
        use rpi_ai::{Api, Model};
        use rpi_harness::agent_harness::AgentLane;
        use rpi_runtime::{ModelRequest, ProviderDeclaration, ProviderRegistry, RuntimeConfig};
        use std::collections::BTreeMap;

        let model = Model::new(
            "embedded-test",
            "Embedded Test",
            Api::Faux,
            "faux",
            "http://localhost:0",
        );
        let mut providers = ProviderRegistry::default();
        providers.declare(ProviderDeclaration {
            id: "faux".into(),
            models: vec![model.clone()],
            api_key_env: None,
            requires_auth: false,
        });
        providers.register(
            FauxProvider::with_models(
                FauxScript::new().with_text("Hello from embedded."),
                vec![model],
            ),
            Api::Faux,
        );
        let resolved = RuntimeConfig::default()
            .resolve(
                &ModelRequest {
                    provider: Some("faux"),
                    model: Some("embedded-test"),
                    ..Default::default()
                },
                &providers,
                &BTreeMap::<String, String>::new(),
            )
            .expect("faux model resolves");

        let dir = tempfile::tempdir().expect("temp dir");
        let mut args = Args::default();
        args.mode = Mode::Rpc;
        args.no_session = true;
        args.no_extensions = true;
        args.offline = true;
        args.no_skills = true;
        args.no_prompt_templates = true;
        args.no_context_files = true;
        let (harness, mut event_rx, _reload) =
            rpi_cli::session::build(&resolved, &args, dir.path(), true)
                .await
                .expect("faux harness builds");
        let lane: Arc<dyn AgentLane> = Arc::new(harness);
        let prompt_lane = Arc::clone(&lane);
        let prompt =
            tokio::spawn(async move { prompt_lane.prompt_text("hello", Vec::new()).await });

        let mapped_tx = EventQueue::default();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, event_rx.recv()).await {
                Ok(Ok(event)) => {
                    let end = matches!(event, RpiEvent::AgentEnd { .. });
                    forward_event(event, &mapped_tx).await;
                    if end {
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = prompt.await.expect("prompt task joins");
        let (events, more) = mapped_tx.drain();
        assert!(!more);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, SessionEvent::Text(text) if text.contains("Hello"))),
            "expected streamed text, got {events:?}"
        );
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, SessionEvent::Done)),
            "the event projector must leave completion to the prompt task: {events:?}"
        );
    }

    #[test]
    fn a_missing_model_reports_a_fatal_error_and_closes() {
        let mut session = EmbeddedSession::spawn(SessionOptions {
            cwd: std::env::temp_dir(),
            session_dir: None,
            provider: Some("definitely-not-a-provider".into()),
            model: Some("missing-model".into()),
            session_id: None,
            append_system_prompt: None,
            provider_request_hook: None,
            delta_arrival_hook: None,
            phase_hook: None,
            before_tool_call: None,
            user_input: false,
        })
        .expect("worker starts");

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut events = Vec::new();
        while std::time::Instant::now() < deadline {
            events.extend(session.poll());
            if events
                .iter()
                .any(|event| matches!(event, SessionEvent::Closed))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            events
                .iter()
                .any(|event| matches!(event, SessionEvent::Error(_))),
            "expected a fatal error, got {events:?}"
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, SessionEvent::Done)));
        assert!(events
            .iter()
            .any(|event| matches!(event, SessionEvent::Closed)));
    }
}
